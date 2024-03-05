use clap::{Arg, Command};
use near_chain::store_validator::StoreValidator;
use near_chain_configs::GenesisValidationMode;
use near_epoch_manager::{
    shard_tracker::{ShardTracker, TrackedConfig},
    EpochManager,
};
use near_o11y::testonly::init_integration_logger;
use framework::{get_default_home, load_config};
use std::path::PathBuf;
use std::process;
use yansi::Color::{Green, Red, White, Yellow};

fn main() {
    init_integration_logger();

    let matches = Command::new("store-validator")
        .arg(
            Arg::new("home")
                .long("home")
                .default_value(get_default_home().into_os_string())
                .value_parser(clap::value_parser!(PathBuf))
                .help("Directory for config and data (default \"~/.near\")")
                .action(clap::ArgAction::Set),
        )
        .subcommand(Command::new("validate"))
        .get_matches();

    let home_dir = matches.get_one::<PathBuf>("home").unwrap();
    let near_config = load_config(home_dir, GenesisValidationMode::Full)
        .unwrap_or_else(|e| panic!("Error loading config: {:#}", e));

    let store = near_store::NodeStorage::opener(
        home_dir,
        near_config.config.archive,
        &near_config.config.store,
        None,
    )
    .open()
    .unwrap()
    .get_hot_store();
    let epoch_manager = EpochManager::new_arc_handle(store.clone(), &near_config.genesis.config);
    let shard_tracker = ShardTracker::new(
        TrackedConfig::from_config(&near_config.client_config),
        epoch_manager.clone(),
    );
    let runtime = framework::NightshadeRuntime::from_config(
        home_dir,
        store.clone(),
        &near_config,
        epoch_manager.clone(),
    );

    let mut store_validator = StoreValidator::new(
        near_config.validator_signer.as_ref().map(|x| x.validator_id().clone()),
        near_config.genesis.config,
        epoch_manager,
        shard_tracker,
        runtime,
        store,
        false,
    );
    store_validator.validate();

    if store_validator.tests_done() == 0 {
        println!("{}", Red.style().bold().paint("No conditions has been validated"));
        process::exit(1);
    }
    println!(
        "{} {}",
        White.style().bold().paint("Conditions validated:"),
        Green.style().bold().paint(store_validator.tests_done().to_string())
    );
    for error in store_validator.errors.iter() {
        println!(
            "{}  {}  {}",
            Red.style().bold().paint(&error.col),
            Yellow.style().bold().paint(&error.key),
            error.err
        );
    }
    if store_validator.is_failed() {
        println!(
            "Errors found: {}",
            Red.style().bold().paint(store_validator.num_failed().to_string())
        );
        process::exit(1);
    } else {
        println!("{}", Green.style().bold().paint("No errors found"));
    }
}
