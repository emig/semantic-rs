#![cfg_attr(feature = "dev", allow(unstable_features))]
#![cfg_attr(feature = "dev", feature(plugin))]
#![cfg_attr(feature = "dev", plugin(clippy))]

mod logger;
mod toml_file;
mod git;
mod changelog;
mod commit_analyzer;
mod cargo;
mod error;
mod github;
mod config;
mod utils;

extern crate rustc_serialize;
extern crate toml;
extern crate regex;
extern crate semver;
extern crate docopt;
extern crate git2;
extern crate clog;
extern crate hyper;
extern crate hubcaps;
extern crate url;
extern crate travis_after_all;
extern crate env_logger;

use docopt::Docopt;
use commit_analyzer::CommitType;
use config::ConfigBuilder;
use std::process;
use semver::Version;
use std::{env,fs};
use std::path::Path;
use std::error::Error;
use std::thread;
use std::time::Duration;
use travis_after_all::Build;
use utils::user_repo_from_url;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");
const USERAGENT: &'static str = concat!("semantic-rs/", env!("CARGO_PKG_VERSION"));
const USAGE: &'static str = "
semantic.rs 🚀

Usage:
  semantic-rs [options]
  semantic-rs --version

Options:
  -h --help              Show this screen.
  --version              Show version.
  -p PATH, --path=PATH   Specifies the repository path. [default: .]
  -w W, --write=W        Write changes to files (default: yes if CI is set, otherwise no).
  -r R, --release=R      Create release on GitHub and publish on crates.io (only in write mode) [default: yes].
  -b B, --branch=B       The branch on which releases should happen. [default: master].
";

const COMMITTER_ERROR_MESSAGE: &'static str = r"
A release commit needs a committer name and email address.
We tried fetching it from different locations, but couldn't find one.

Committer information is taken from the following environment variables, if set:

GIT_COMMITTER_NAME
GIT_COMMITTER_EMAIL

If none is set the normal git config is tried in the following order:

Local repository config
User config
Global config";

macro_rules! print_exit {
    ($fmt:expr) => {{
        logger::stderr($fmt);
        process::exit(1);
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        logger::stderr(format!($fmt, $($arg)*));
        process::exit(1);
    }};
}

#[derive(Debug, RustcDecodable)]
struct Args {
    flag_path: String,
    flag_write: Option<String>,
    flag_version: bool,
    flag_release: String,
    flag_branch: String,
}

fn string_to_bool(answer: &str) -> bool {
    match &answer.to_lowercase()[..] {
        "yes" | "true" | "1" => true,
        _ => false
    }
}

fn version_bump(version: &Version, bump: CommitType) -> Option<Version> {
    let mut version = version.clone();
    match bump {
        CommitType::Unknown => return None,
        CommitType::Patch => version.increment_patch(),
        CommitType::Minor => version.increment_minor(),
        CommitType::Major => version.increment_major(),
    }

    Some(version)
}

fn ci_env_set() -> bool {
    env::var("CI").is_ok()
}

fn current_branch(repo: &git2::Repository) -> Option<String> {
    if let Ok(branch) = env::var("TRAVIS_BRANCH") {
        return Some(branch)
    }

    let head = repo.head().expect("No HEAD found for repository");

    if head.is_branch() {
        let short = head.shorthand().expect("No branch name found");
        return Some(short.into());
    }

    None
}

fn is_release_branch(current: &str, release: &str) -> bool {
    if let Ok(pr) = env::var("TRAVIS_PULL_REQUEST") {
        if pr != "false" {
            return false;
        }
    }

    current == release
}

fn push_to_github(config: &config::Config, tag_name: &str) {
    logger::stdout("Pushing new commit and tag");
    git::push(&config, &tag_name)
        .unwrap_or_else(|err| print_exit!("Failed to push git: {:?}", err));

    logger::stdout("Waiting a tiny bit, so GitHub can store the git tag");
    thread::sleep(Duration::from_secs(1));
}

fn release_on_github(config: &config::Config, tag_message: &str, tag_name: &str) {
    if github::can_release(&config) {
        logger::stdout("Creating GitHub release");
        github::release(&config, &tag_name, &tag_message)
            .unwrap_or_else(|err| print_exit!("Failed to create GitHub release: {:?}", err));
    } else {
        logger::stdout("Project not hosted on GitHub. Skipping release step");
    }
}

fn release_on_cratesio(config: &config::Config) {
    logger::stdout("Publishing crate on crates.io");
    if !cargo::publish(&config.repository_path, &config.cargo_token.as_ref().unwrap()) {
        print_exit!("Failed to publish on crates.io");
    }
}

fn generate_changelog(repository_path: &str, version: &Version, new_version: &String) -> String {
    logger::stdout(format!("New version would be: {}", new_version));
    logger::stdout("Would write the following Changelog:");
    match changelog::generate(repository_path, &version.to_string(), new_version) {
        Ok(log) => log,
        Err(err) => {
            logger::stderr(format!("Generating Changelog failed: {:?}", err));
            process::exit(1)
        }
    }
}

fn write_changelog(repository_path: &str, version: &Version, new_version: &str) {
    logger::stdout("Writing Changelog");
    changelog::write(repository_path, &version.to_string(), &new_version)
        .unwrap_or_else(|err| print_exit!("Writing Changelog failed: {:?}", err));
}

fn print_changelog(changelog: &str) {
    logger::stdout("====================================");
    logger::stdout(changelog);
    logger::stdout("====================================");
    logger::stdout("Would create annotated git tag");
}

fn package_crate(config: &config::Config, repository_path: &str, new_version: &str) {
    if config.release_mode {
        logger::stdout("Updating lockfile");
        if !cargo::update_lockfile(repository_path) {
            print_exit!("`cargo fetch` failed. See above for the cargo error message.");
        }
    }

    git::commit_files(&config, &new_version)
        .unwrap_or_else(|err| print_exit!("Committing files failed: {:?}", err));

    logger::stdout("Package crate");
    if !cargo::package(repository_path) {
        print_exit!("`cargo package` failed. See above for the cargo error message.");
    }
}

fn main() {
    env_logger::init().expect("Can't instantiate env logger");

    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.decode())
        .unwrap_or_else(|e| e.exit());

    let mut config_builder = ConfigBuilder::new();

    if args.flag_version {
        println!("semantic.rs 🚀 -- v{}", VERSION);
        process::exit(0);
    }

    // If write mode is requested OR denied,
    // adhere to the user's wish,
    // otherwise we decide based on whether we are running in CI.
    let write_mode = match args.flag_write {
        None => ci_env_set(),
        Some(ref flag) => string_to_bool(flag)
    };

    // We can only release, if we are allowed to write
    let release_mode = write_mode && string_to_bool(&args.flag_release);

    config_builder.write(write_mode);
    config_builder.release(release_mode);
    config_builder.branch(args.flag_branch);

    println!("semantic.rs 🚀");

    logger::stdout("Analyzing your repository");
    let path = Path::new(&args.flag_path);
    let path = fs::canonicalize(path)
        .unwrap_or_else(|_| print_exit!("Path does not exist or a component is not a directory"));
    let repository_path = path.to_str()
        .unwrap_or_else(|| print_exit!("Path is not valid unicode"));

    let repo = match git2::Repository::open(repository_path) {
        Ok(repo) => repo,
        Err(e) => {
            logger::stderr(format!("Could not open the git repository: {:?}", e));
            process::exit(1);
        }
    };

    config_builder.repository_path(repository_path.to_owned());

    // extra scope scope to make sure borrow of `repo` is dropped
    {
        let signature = match git::get_signature(&repo) {
            Ok(sig) => sig,
            Err(e) => {
                logger::stderr(format!("Failed to get the committer's name and email address: {}", e.description()));
                logger::stderr(COMMITTER_ERROR_MESSAGE);
                process::exit(1);
            }
        };

        config_builder.signature(signature.to_owned());
    }

    // In case we are in write-mode AND release mode,
    // we will make sure we got all configuration settings
    if write_mode && release_mode {
        let remote_or_none = repo.find_remote("origin");
        match remote_or_none {
            Ok(remote) => {
                let url = remote.url().expect("Remote URL is not valid UTF-8").to_owned();
                let (user, repo_name) = user_repo_from_url(&url)
                    .unwrap_or_else(|e| print_exit!("Could not extract user and repository name from URL: {:?}", e));
                config_builder.user(user);
                config_builder.repository_name(repo_name);

                if github::is_github_url(&url) {
                    let gh_token = env::var("GH_TOKEN")
                        .unwrap_or_else(|err| print_exit!("GH_TOKEN not set: {:?}", err));
                    config_builder.gh_token(gh_token);
                }

                let cargo_token = env::var("CARGO_TOKEN")
                    .unwrap_or_else(|err| print_exit!("CARGO_TOKEN not set: {:?}", err));

                config_builder.cargo_token(cargo_token);
            },
            Err(err) => {
                logger::warn(format!("Could not determine the origin remote url: {:?}", err));
                logger::warn("semantic-rs can't push changes or create a release on GitHub");
            }
        }
    }

    config_builder.repository(repo);
    let config = config_builder.build();

    let branch = current_branch(&config.repository)
        .unwrap_or_else(|| print_exit!("Could not determine current branch."));

    if !is_release_branch(&branch, &config.branch) {
        println!("Current branch is '{}', releases are only done from branch '{}'", branch, config.branch);
        println!("No release done from a pull request either.");
        process::exit(0);
    }

    if config.release_mode && ci_env_set() {
        let build_run = Build::from_env()
            .unwrap_or_else(|e| print_exit!("CI mode, but can't check other builds. Error: {:?}", e));

        if !build_run.is_leader() {
            println!("Not the build leader. Nothing to do. Bye.");
            process::exit(0);
        }

        println!("I am the build leader. Waiting for other jobs to finish.");
        match build_run.wait_for_others() {
            Ok(()) => println!("Other jobs finished and succeeded. Doing my work now."),
            Err(travis_after_all::Error::FailedBuilds) => {
                print_exit!("Some builds failed. Stopping here.");
            },
            Err(e) => print_exit!("Waiting for other builds failed Reason: {:?}", e),
        }
    }

    let version = toml_file::read_from_file(&config.repository_path)
        .unwrap_or_else(|err| print_exit!("Reading `Cargo.toml` failed: {:?}", err));

    let version = Version::parse(&version).expect("Not a valid version");
    logger::stdout(format!("Current version: {}", version.to_string()));

    logger::stdout("Analyzing commits");

    let bump = git::version_bump_since_latest(&config.repository);
    if write_mode {
        logger::stdout(format!("Commits analyzed. Bump will be {:?}", bump));
    } else {
        logger::stdout(format!("Commits analyzed. Bump would be {:?}", bump));
    }
    let new_version = match version_bump(&version, bump) {
        Some(new_version) => new_version.to_string(),
        None => {
            logger::stdout("No version bump. Nothing to do.");
            process::exit(0);
        }
    };

    if !config.write_mode {
        let changelog = generate_changelog(repository_path, &version, &new_version);
        print_changelog(&changelog);
    } else {
        logger::stdout(format!("New version: {}", new_version));

        toml_file::write_new_version(repository_path, &new_version)
            .unwrap_or_else(|err| print_exit!("Writing `Cargo.toml` failed: {:?}", err));

        write_changelog(&repository_path, &version, &new_version);
        package_crate(&config, &repository_path, &new_version);

        logger::stdout("Creating annotated git tag");
        let tag_message = changelog::generate(repository_path, &version.to_string(), &new_version)
            .unwrap_or_else(|err| print_exit!("Can't generate changelog: {:?}", err));

        let tag_name = format!("v{}", new_version);
        git::tag(&config, &tag_name, &tag_message)
            .unwrap_or_else(|err| print_exit!("Failed to create git tag: {:?}", err));

        if config.release_mode && config.can_push() {
            push_to_github(&config, &tag_name);
            release_on_github(&config, &tag_message, &tag_name);
            release_on_cratesio(&config);
            println!("{} v{} is released. 🚀🚀🚀", config.repository_name.unwrap(), new_version);
        }
    }
}
