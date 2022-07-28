use std::{
    collections::HashMap,
    fs::{create_dir_all, File},
    io::Write,
};

use colored::Colorize;
use rug::{ops::Pow, Float};
use winterfell::{
    crypto::hashers::Poseidon,
    math::{fields::f256::BaseElement, log2, StarkField},
    Air, HashFunction, Prover,
};

use crate::{
    json::proof_to_json,
    utils::{
        canonicalize, check_file, command_execution, delete_directory, delete_file, Executable,
        LoggingLevel, WinterCircomError,
    },
    WinterPublicInputs,
};

/// Verify a circom proof.
///
/// Requires the `verification_key.json`, `proof.json` and `public.json` files
/// to be present in the directory `target/circom/<circuit_name>`. These files
/// can be generated by the [circom_prove] function.
///
/// [Verbose](LoggingLevel::Verbose) logging level is recommended.
pub fn circom_verify(
    circuit_name: &str,
    logging_level: LoggingLevel,
) -> Result<(), WinterCircomError> {
    check_file(
        format!("target/circom/{}/verification_key.json", circuit_name),
        Some("needed for verification"),
    )?;
    check_file(
        format!("target/circom/{}/public.json", circuit_name),
        Some("needed for verification"),
    )?;
    check_file(
        format!("target/circom/{}/proof.json", circuit_name),
        Some("needed for verification"),
    )?;

    command_execution(
        Executable::SnarkJS,
        &["g16v", "verification_key.json", "public.json", "proof.json"],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )
}

/// Prepare verification of a Winterfell proof by a Circom circuit.
///
/// The following steps are executed:
///
/// - Generate the proof
/// - (Not in release mode) Verify the proof
/// - Parse the proof into a Circom-compatible JSON object
/// - Print the JSON proof to a file
/// - Generate Circom code containing the parameters of the verification
/// - Compute execution witness
/// - Generate circuit-specific keys
/// - Generate proof
pub fn circom_prove<P>(
    prover: P,
    trace: <P as Prover>::Trace,
    circuit_name: &str,
    logging_level: LoggingLevel,
) -> Result<(), WinterCircomError>
where
    P: Prover<BaseField = BaseElement>,
    <<P as Prover>::Air as Air>::PublicInputs: WinterPublicInputs,
{
    // BUILD PROOF
    // ===========================================================================

    if logging_level.print_big_steps() {
        println!("{}", "Building STARK proof...".green());
    }

    assert_eq!(prover.options().hash_fn(), HashFunction::Poseidon);

    let pub_inputs = prover.get_pub_inputs(&trace);
    let proof = prover
        .prove(trace)
        .map_err(|e| WinterCircomError::ProverError(e))?;

    // VERIFY PROOF
    // ===========================================================================

    #[cfg(debug_assertions)]
    {
        if logging_level.print_big_steps() {
            println!("{}", "Verifying STARK proof...".green());
        }

        winterfell::verify::<P::Air>(proof.clone(), pub_inputs.clone())
            .map_err(|err| WinterCircomError::InvalidProof(Some(err)))?;
    }

    // BUILD JSON OUTPUTS
    // ===========================================================================

    if logging_level.print_big_steps() {
        println!("{}", "Parsing proof to JSON...".green());
    }

    // retrieve air and proof options
    let air = P::Air::new(
        proof.get_trace_info(),
        pub_inputs.clone(),
        proof.options().clone(),
    );

    // convert proof to json object
    let mut fri_tree_depths = Vec::new();
    let json = proof_to_json::<P::Air, Poseidon<BaseElement>>(
        proof,
        &air,
        pub_inputs.clone(),
        &mut fri_tree_depths,
    );

    // print json to file
    let json_string = format!("{}", json);
    create_dir_all(format!("target/circom/{}", circuit_name)).map_err(|e| {
        WinterCircomError::IoError {
            io_error: e,
            comment: Some(String::from("creating Circom output directory")),
        }
    })?;
    let mut file =
        File::create(format!("target/circom/{}/input.json", circuit_name)).map_err(|e| {
            WinterCircomError::IoError {
                io_error: e,
                comment: Some(String::from("creating input.json")),
            }
        })?;
    file.write(&json_string.into_bytes())
        .map_err(|err| WinterCircomError::IoError {
            io_error: err,
            comment: Some(String::from("writing input.json")),
        })?;

    // CIRCOM MAIN
    // ===========================================================================

    if logging_level.print_big_steps() {
        println!("{}", "Generating Circom code...".green());
    }

    generate_circom_main::<P::BaseField, P::Air>(
        circuit_name,
        &air,
        &fri_tree_depths,
        json["pub_coin_seed"].as_array().unwrap().len(),
    )?;

    // compile circom
    if logging_level.print_big_steps() {
        println!("{}", "Compiling Circom code...".green());
    }

    delete_file(format!("target/circom/{}/verifier.r1cs", circuit_name));
    delete_directory(format!("target/circom/{}/verifier_cpp", circuit_name));
    command_execution(
        Executable::Circom,
        &["--r1cs", "--c", "verifier.circom"],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/verifier.r1cs", circuit_name),
        Some("circom command must have failed"),
    )?;

    // generate witness
    if logging_level.print_big_steps() {
        println!("{}", "Generating witness...".green());
    }

    command_execution(
        Executable::Make,
        &[],
        Some(&format!("target/circom/{}/verifier_cpp", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/verifier_cpp/verifier", circuit_name),
        Some("make command must have failed"),
    )?;

    delete_file(format!("target/circom/{}/witness.wtns", circuit_name));
    command_execution(
        Executable::Custom {
            path: format!("target/circom/{}/verifier_cpp/verifier", circuit_name),
            verbose_argument: None,
        },
        &["input.json", "witness.wtns"],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/witness.wtns", circuit_name),
        Some("witness generation must have failed"),
    )?;

    // generate circuit key
    if logging_level.print_big_steps() {
        println!("{}", "Generating circuit-specific key...".green());
    }

    delete_file(format!("target/circom/{}/verifier.zkey", circuit_name));
    check_file(
        String::from("final.ptau"),
        Some("needed for circuit-specific key generation"),
    )?;
    command_execution(
        Executable::SnarkJS,
        &[
            "g16s",
            "verifier.r1cs",
            "../../../final.ptau",
            "verifier.zkey",
        ],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/verifier.zkey", circuit_name),
        Some("circuit-specific key generation must have failed"),
    )?;

    /*
    delete_file(format!("target/circom/{}/verifier_0001.zkey", circuit_name))?;
    command_execution(
        canonicalize("iden3/snarkjs/build/cli.cjs")?,
        &[
            "zkc",
            "verifier_0000.zkey",
            "verifier_0001.zkey",
            // 25 random alphanumeric characters
            // TODO: make it work for Windows as well
            "-e=$(head/dev/urandom | tr -dc a-zA-Z0-9 | head -c 25)",
        ],
        Some(&format!("target/circom/{}", circuit_name)),
    )?;
    check_file(
        format!("target/circom/{}/verifier_0001.zkey", circuit_name),
        Some("circuit-specific key contribution must have failed"),
    )?;
    */

    delete_file(format!(
        "target/circom/{}/verification_key.json",
        circuit_name
    ));
    command_execution(
        Executable::SnarkJS,
        &["zkev", "verifier.zkey", "verification_key.json"],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/verification_key.json", circuit_name),
        Some("verification key export must have failed"),
    )?;

    // generate snark proof
    if logging_level.print_big_steps() {
        println!("{}", "Generating SNARK proof...".green());
    }

    delete_file(format!("target/circom/{}/proof.json", circuit_name));
    delete_file(format!("target/circom/{}/public.json", circuit_name));
    command_execution(
        Executable::SnarkJS,
        &[
            "g16p",
            "verifier.zkey",
            "witness.wtns",
            "proof.json",
            "public.json",
        ],
        Some(&format!("target/circom/{}", circuit_name)),
        &logging_level,
    )?;
    check_file(
        format!("target/circom/{}/public.json", circuit_name),
        Some("proof must have failed"),
    )?;
    check_file(
        format!("target/circom/{}/proof.json", circuit_name),
        Some("proof must have failed"),
    )?;

    if logging_level.print_big_steps() {
        println!("{}", "Proof generated successfully!".green());
        println!(
            "Proof file:        {}",
            canonicalize(format!("target/circom/{}/proof.json", circuit_name))?.to_string_lossy()
        );
        println!(
            "Verification key:  {}",
            canonicalize(format!(
                "target/circom/{}/verification_key.json",
                circuit_name
            ))?
            .to_string_lossy()
        );
        println!(
            "Public in/outputs: {}",
            canonicalize(format!("target/circom/{}/public.json", circuit_name))?.to_string_lossy()
        );
    }

    Ok(())
}

/// Generate a circom main file that defines the parameters for verifying a proof.
///
/// The main file is generated in the `target/circom/<circuit_name>/` directory,
/// with the `verifier.circom` name.
pub fn generate_circom_main<E, AIR>(
    circuit_name: &str,
    air: &AIR,
    fri_tree_depths: &Vec<usize>,
    pub_coin_seed_len: usize,
) -> Result<(), WinterCircomError>
where
    E: StarkField,
    AIR: Air,
    AIR::PublicInputs: WinterPublicInputs,
{
    let fri_tree_depths = if fri_tree_depths.len() == 0 {
        String::from("[0]")
    } else {
        format!(
            "[{}]",
            fri_tree_depths
                .iter()
                .map(|x| format!("{}", x))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    let mut file = File::create(format!("target/circom/{}/verifier.circom", circuit_name))
        .map_err(|e| WinterCircomError::IoError {
            io_error: e,
            comment: Some(String::from("trying to create circom main file")),
        })?;

    let arguments = format!(
        "{}, // addicity\n    \
            {}, // ce_blowup_factor\n    \
            {}, // domain_offset\n    \
            {}, // folding_factor\n    \
            {}, // fri_tree_depth\n    \
            {}, // grinding_factor\n    \
            {}, // lde_blowup_factor\n    \
            {}, // num_assertions\n    \
            {}, // num_draws\n    \
            {}, // num_fri_layers\n    \
            {}, // num_pub_coin_seed\n    \
            {}, // num_public_inputs\n    \
            {}, // num_queries\n    \
            {}, // num_transition_constraints\n    \
            {}, // trace_length\n    \
            {},  // trace_length\n    \
            {} // tree_depth",
        E::TWO_ADICITY,
        air.ce_blowup_factor(),
        air.domain_offset(),
        air.options().to_fri_options().folding_factor(),
        fri_tree_depths,
        air.options().grinding_factor(),
        air.options().blowup_factor(),
        air.context().num_assertions(),
        number_of_draws(
            air.options().num_queries() as u128,
            air.lde_domain_size() as u128,
            128
        ),
        air.options()
            .to_fri_options()
            .num_fri_layers(air.lde_domain_size()),
        pub_coin_seed_len,
        AIR::PublicInputs::NUM_PUB_INPUTS,
        air.options().num_queries(),
        air.context().num_transition_constraints(),
        air.trace_length(),
        air.trace_info().width(),
        log2(air.lde_domain_size()),
    );

    let file_contents = format!(
        "pragma circom 2.0.0;\n\
        \n\
        include \"../../../circuits/verify.circom\";\n\
        include \"../../../circuits/air/{}.circom\";\n\
        \n\
        component main {{public [ood_frame_constraint_evaluation, ood_trace_frame]}} = Verify(\n    \
            {}\n\
        );\n\
",
        circuit_name, arguments
    );

    file.write(file_contents.as_bytes())
        .map_err(|e| WinterCircomError::IoError {
            io_error: e,
            comment: Some(String::from("trying to write to circom main file")),
        })?;

    Ok(())
}

// HELPER FUNCTIONS
// ===========================================================================

fn number_of_draws(num_queries: u128, lde_domain_size: u128, security: i32) -> u128 {
    let mut num_draws: u128 = 0;
    let precision: u32 = security as u32 + 2;

    while {
        let st = step(
            0,
            num_draws,
            &mut HashMap::new(),
            num_queries,
            lde_domain_size,
            security,
        );
        num_draws += 1;
        1 - st > Float::with_val(precision, 2_f64).pow(-security)
    } {}

    num_draws
}

fn step(
    x: u128,
    n: u128,
    memo: &mut HashMap<(u128, u128), Float>,
    num_queries: u128,
    lde_domain_size: u128,
    security: i32,
) -> Float {
    let precision: u32 = security as u32 + 2;
    match memo.get(&(x, n)) {
        Some(val) => val.clone(),
        None => {
            let num: Float;
            if x == num_queries {
                num = Float::with_val(precision, 1f64);
            } else if n == 0 {
                num = Float::with_val(precision, 0f64);
            } else {
                let a = step(x + 1, n - 1, memo, num_queries, lde_domain_size, security);
                let b = step(x, n - 1, memo, num_queries, lde_domain_size, security);
                num = Float::with_val(precision, lde_domain_size - x)
                    / (Float::with_val(precision, lde_domain_size))
                    * a
                    + Float::with_val(precision, x) / (Float::with_val(precision, lde_domain_size))
                        * b;
            }
            memo.insert((x, n), num.clone());
            num
        }
    }
}
