//! Release-engineering CLI: hash-bound signing of koe update metadata.
//!
//! Used by the release workflow to produce the signed, versioned, expiring
//! metadata that `koe update apply` verifies before installing an update.
//! Never used at runtime by the app; the signing seed is a release secret.
//!
//! ```text
//! KOE_UPDATE_SIGNING_SEED_HEX=<secret> koe-release-sign keys
//! KOE_UPDATE_SIGNING_SEED_HEX=<secret> koe-release-sign sign --app-version <v>
//!     --platform <triple> --install-target <relative-path> --expires-unix-s <s>
//!     --metadata-version <n> --artifact-dir <dir> [--out <file>]
//! koe-release-sign verify --metadata <file> --public-key <hex>
//!     --expected-platform <triple> [--now-unix-s <s>]
//! ```

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand};
use koe_update::{
    MAX_METADATA_BYTES, MAX_TARGETS, UpdateError, UpdateMetadata, UpdateTarget, file_digest,
    hex_decode, parse_signed_update, public_key_hex, sign_update, validate_metadata,
    verify_signature,
};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "koe-release-sign",
    about = "Sign and verify koe update metadata for releases"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Derive the public key id for a signing seed (safe to publish).
    Keys,
    /// Hash every artifact in a directory, produce and sign the metadata.
    Sign {
        #[arg(long)]
        app_version: String,
        /// Canonical rustc target triple, e.g. `x86_64-apple-darwin`.
        #[arg(long)]
        platform: String,
        /// Relative path of the executable target accepted by the launcher.
        #[arg(long)]
        install_target: String,
        /// Unix seconds after which the metadata must not be accepted.
        #[arg(long)]
        expires_unix_s: u64,
        /// Monotonic metadata version; strictly increasing across releases.
        #[arg(long)]
        metadata_version: u64,
        /// Directory whose relative files become the hash-bound targets.
        #[arg(long)]
        artifact_dir: PathBuf,
        /// Optional output file; default is stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify a signed metadata document against a pinned public key.
    Verify {
        #[arg(long)]
        metadata: PathBuf,
        #[arg(long)]
        public_key: String,
        #[arg(long)]
        expected_platform: String,
        #[arg(long)]
        now_unix_s: Option<u64>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let code = error.code();
            let _ignored = writeln!(
                io::stderr(),
                "{{\"code\":\"{code}\",\"message\":\"{error}\"}}"
            );
            ExitCode::FAILURE
        },
    }
}

fn execute(cli: &Cli) -> Result<(), UpdateError> {
    match &cli.command {
        Command::Keys => {
            let seed = signing_seed()?;
            println!("{}", public_key_hex(&seed));
            Ok(())
        },
        Command::Sign {
            app_version,
            platform,
            install_target,
            expires_unix_s,
            metadata_version,
            artifact_dir,
            out,
        } => {
            let seed = signing_seed()?;
            let targets = hash_directory(artifact_dir)?;
            if targets.is_empty()
                || *metadata_version == 0
                || *expires_unix_s == 0
                || app_version.is_empty()
                || platform.is_empty()
                || !targets.iter().any(|target| target.path == *install_target)
            {
                return Err(UpdateError::InvalidMetadata);
            }
            let payload = UpdateMetadata {
                schema_version: koe_update::METADATA_SCHEMA,
                role: "targets".to_owned(),
                version: *metadata_version,
                expires_at_unix_s: *expires_unix_s,
                app_version: app_version.clone(),
                platform: platform.clone(),
                install_target: install_target.clone(),
                targets,
            };
            validate_metadata(&payload, platform, 0)?;
            let signed = sign_update(&payload, &seed)?;
            let mut bytes =
                serde_json::to_vec_pretty(&signed).map_err(|_| UpdateError::StoreFailed)?;
            bytes.push(b'\n');
            write_output(out.as_ref(), &bytes)?;
            Ok(())
        },
        Command::Verify {
            metadata,
            public_key,
            expected_platform,
            now_unix_s,
        } => {
            let file = fs::File::open(metadata).map_err(|_| UpdateError::StoreFailed)?;
            if file.metadata().map_err(|_| UpdateError::StoreFailed)?.len() > MAX_METADATA_BYTES {
                return Err(UpdateError::InvalidMetadata);
            }
            let mut bytes = Vec::new();
            file.take(MAX_METADATA_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|_| UpdateError::StoreFailed)?;
            let signed = parse_signed_update(&bytes)?;
            verify_signature(&signed, public_key)?;
            validate_metadata(
                &signed.payload,
                expected_platform,
                now_unix_s.unwrap_or_else(current_unix_s),
            )?;
            println!(
                "signature valid for app {} version {}",
                signed.payload.app_version, signed.payload.version
            );
            Ok(())
        },
    }
}

fn signing_seed() -> Result<[u8; 32], UpdateError> {
    let seed_hex =
        std::env::var("KOE_UPDATE_SIGNING_SEED_HEX").map_err(|_| UpdateError::InvalidKey)?;
    let bytes = hex_decode(&seed_hex)?;
    if bytes.len() == 32 {
        Ok(<[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| UpdateError::InvalidKey)?)
    } else {
        Err(UpdateError::InvalidKey)
    }
}

fn current_unix_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn hash_directory(directory: &Path) -> Result<Vec<UpdateTarget>, UpdateError> {
    let metadata = fs::symlink_metadata(directory).map_err(|_| UpdateError::StoreFailed)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UpdateError::PathRejected);
    }
    let mut targets = Vec::new();
    collect_files(directory, directory, &mut targets)?;
    targets.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(targets)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    targets: &mut Vec<UpdateTarget>,
) -> Result<(), UpdateError> {
    for entry in fs::read_dir(directory).map_err(|_| UpdateError::StoreFailed)? {
        let entry = entry.map_err(|_| UpdateError::StoreFailed)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| UpdateError::StoreFailed)?;
        if file_type.is_dir() && !file_type.is_symlink() {
            collect_files(root, &path, targets)?;
        } else if file_type.is_file() && !file_type.is_symlink() {
            let (sha256, size) = file_digest(&path)?;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| UpdateError::PathRejected)?;
            let components = relative
                .iter()
                .map(|component| component.to_str().ok_or(UpdateError::PathRejected))
                .collect::<Result<Vec<_>, _>>()?;
            if targets.len() >= MAX_TARGETS {
                return Err(UpdateError::InvalidMetadata);
            }
            targets.push(UpdateTarget {
                path: components.join("/"),
                sha256,
                size,
            });
        }
    }
    Ok(())
}

fn write_output(
    out: Option<&PathBuf>,
    bytes: &[u8],
) -> Result<(), UpdateError> {
    if let Some(path) = out {
        let parent = path.parent().ok_or(UpdateError::PathRejected)?;
        let file_name = path.file_name().ok_or(UpdateError::PathRejected)?;
        let temporary = parent.join(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            Uuid::new_v4()
        ));
        let result = (|| -> Result<(), UpdateError> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = options
                .open(&temporary)
                .map_err(|_| UpdateError::StoreFailed)?;
            file.write_all(bytes)
                .map_err(|_| UpdateError::StoreFailed)?;
            file.sync_all().map_err(|_| UpdateError::StoreFailed)?;
            fs::hard_link(&temporary, path).map_err(|_| UpdateError::Conflict)?;
            fs::remove_file(&temporary).map_err(|_| UpdateError::StoreFailed)
        })();
        if result.is_err() {
            let _ignored = fs::remove_file(&temporary);
        }
        result
    } else {
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        handle
            .write_all(bytes)
            .map_err(|_| UpdateError::StoreFailed)?;
        handle.flush().map_err(|_| UpdateError::StoreFailed)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{hash_directory, write_output};
    use koe_update::UpdateError;

    #[test]
    fn hashing_is_recursive_sorted_and_excludes_symlinks() {
        let root = TempDir::new().expect("temp");
        fs::create_dir(root.path().join("nested")).expect("directory");
        fs::write(root.path().join("z"), b"z").expect("z");
        fs::write(root.path().join("nested/a"), b"a").expect("a");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("z"), root.path().join("link"))
            .expect("symlink");
        let targets = hash_directory(root.path()).expect("hash");
        let paths: Vec<_> = targets.iter().map(|target| target.path.as_str()).collect();
        assert_eq!(paths, ["nested/a", "z"]);
    }

    #[test]
    fn output_never_replaces_an_existing_file() {
        let root = TempDir::new().expect("temp");
        let output = root.path().join("metadata.json");
        fs::write(&output, b"existing").expect("fixture");
        assert_eq!(
            write_output(Some(&output), b"replacement"),
            Err(UpdateError::Conflict)
        );
        assert_eq!(fs::read(output).expect("read"), b"existing");
    }
}
