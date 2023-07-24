use anyhow::{anyhow, Result};
use console::user_attended;
use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use lazy_static::lazy_static;
use sha2::{Digest, Sha256};
use std::env::consts::ARCH;
use std::fs::{self, File};
use std::os::unix::prelude::MetadataExt;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub const CURRENT_CIEL_VERSION: usize = 3;
const CURRENT_CIEL_VERSION_STR: &str = "3";
pub const CIEL_DIST_DIR: &str = ".ciel/container/dist";
pub const CIEL_INST_DIR: &str = ".ciel/container/instances";
pub const CIEL_DATA_DIR: &str = ".ciel/data";
const SKELETON_DIRS: &[&str] = &[CIEL_DIST_DIR, CIEL_INST_DIR, CIEL_DATA_DIR];
pub const CIEL_MAINLINE_ARCHS: &[&str] = &["amd64", "arm64", "ppc64el", "mips64r6el", "riscv64"];
pub const CIEL_RETRO_ARCHS: &[&str] = &["armv4", "armv6hf", "armv7hf", "i486", "m68k", "powerpc"];

lazy_static! {
    static ref SPINNER_STYLE: indicatif::ProgressStyle =
        indicatif::ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠸⠴⠦⠇ ")
            .template("{spinner:.green} {wide_msg}")
            .unwrap();
}

#[macro_export]
macro_rules! make_progress_bar {
    ($msg:expr) => {
        concat!(
            "{spinner} [{bar:25.cyan/blue}] ",
            $msg,
            " ({bytes_per_sec}, eta {eta})"
        )
    };
}

#[inline]
pub fn create_spinner(msg: &'static str, tick_rate: u64) -> indicatif::ProgressBar {
    let spinner = indicatif::ProgressBar::new_spinner().with_style(SPINNER_STYLE.clone());
    spinner.set_message(msg);
    spinner.enable_steady_tick(Duration::from_millis(tick_rate));

    spinner
}

#[inline]
pub fn check_arch_name(arch: &str) -> bool {
    CIEL_MAINLINE_ARCHS.contains(&arch) || CIEL_RETRO_ARCHS.contains(&arch)
}

//// Workaround for mips64r6el
#[cfg(feature = "mips64r6")]
#[inline]
pub fn get_arch_name() -> Option<&'static str> {
    Some("mips64r6el")
}

/// AOSC OS specific architecture mapping for ppc64
#[cfg(target_arch = "powerpc64")]
#[inline]
pub fn get_arch_name() -> Option<&'static str> {
    let mut endian: libc::c_int = -1;
    let result = unsafe { libc::prctl(libc::PR_GET_ENDIAN, &mut endian as *mut libc::c_int) };
    if result < 0 {
        return None;
    }
    match endian {
        libc::PR_ENDIAN_LITTLE | libc::PR_ENDIAN_PPC_LITTLE => Some("ppc64el"),
        libc::PR_ENDIAN_BIG => Some("ppc64"),
        _ => None,
    }
}

/// AOSC OS specific architecture mapping table
#[cfg(not(target_arch = "powerpc64"))]
#[cfg(not(feature = "mips64r6"))]
#[inline]
pub fn get_host_arch_name() -> Result<&'static str> {
    match ARCH {
        "x86_64" => Ok("amd64"),
        "x86" => Ok("i486"),
        "powerpc" => Ok("powerpc"),
        "aarch64" => Ok("arm64"),
        "mips64" => Ok("loongson3"),
        "riscv64" => Ok("riscv64"),
        _ => Err(anyhow!(
            "Current host architecture {ARCH} is not supported by Ciel."
        )),
    }
}

/// Calculate the Sha256 checksum of the given stream
pub fn sha256sum<R: Read>(mut reader: R) -> Result<String> {
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher)?;

    Ok(format!("{:x}", hasher.finalize()))
}

/// Extract the given .tar.xz stream and preserve all the file attributes
pub fn extract_tar_xz<R: Read>(reader: R, path: &Path) -> Result<()> {
    let decompress = xz2::read::XzDecoder::new(reader);
    let mut tar_processor = tar::Archive::new(decompress);
    tar_processor.set_unpack_xattrs(true);
    tar_processor.set_preserve_permissions(true);
    tar_processor.unpack(path)?;

    Ok(())
}

pub fn extract_system_tarball(path: &Path, total: u64) -> Result<()> {
    let f = File::open(path)?;
    let progress_bar = indicatif::ProgressBar::new(total);
    progress_bar.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(make_progress_bar!("Extracting tarball..."))
            .unwrap(),
    );
    progress_bar.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(5));
    let reader = progress_bar.wrap_read(f);
    let dist_dir = PathBuf::from(CIEL_DIST_DIR);
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir).ok();
        fs::create_dir_all(&dist_dir)?;
    }
    extract_tar_xz(reader, &dist_dir)?;
    progress_bar.finish_and_clear();

    Ok(())
}

pub fn ciel_init() -> Result<()> {
    for dir in SKELETON_DIRS {
        fs::create_dir_all(dir)?;
    }
    let mut f = File::create(".ciel/version")?;
    f.write_all(CURRENT_CIEL_VERSION_STR.as_bytes())?;

    Ok(())
}

/// Find the ciel directory
pub fn find_ciel_dir<P: AsRef<Path>>(start: P) -> Result<PathBuf> {
    let start_path = fs::metadata(start.as_ref())?;
    let start_dev = start_path.dev();
    let mut current_dir = start.as_ref().to_path_buf();
    loop {
        if !current_dir.exists() {
            return Err(anyhow!("Hit filesystem ceiling!"));
        }
        let current_dev = current_dir.metadata()?.dev();
        if current_dev != start_dev {
            return Err(anyhow!("Hit filesystem boundary!"));
        }
        if current_dir.join(".ciel").is_dir() {
            return Ok(current_dir);
        }
        current_dir = current_dir.join("..");
    }
}

pub fn is_instance_exists(instance: &str) -> bool {
    Path::new(CIEL_INST_DIR).join(instance).is_dir()
}

pub fn is_legacy_workspace() -> Result<bool> {
    let mut f = fs::File::open(".ciel/version")?;
    // TODO: use a more robust check
    let mut buf = [0u8; 1];
    f.read_exact(&mut buf)?;

    Ok(buf[0] < CURRENT_CIEL_VERSION_STR.as_bytes()[0])
}

pub fn ask_for_target_arch() -> Option<&'static str> {
    // Collect all supported architectures
    let host_arch = get_host_arch_name().unwrap();
    if !user_attended() {
        return Some(host_arch);
    }
    let mut all_archs: Vec<&'static str> = CIEL_MAINLINE_ARCHS.into();
    all_archs.append(&mut CIEL_RETRO_ARCHS.into());
    let default_arch_index = all_archs.iter().position(|a| *a == host_arch).unwrap();
    // Setup Dialoguer
    let theme = ColorfulTheme::default();
    let prefixed_archs = CIEL_MAINLINE_ARCHS
        .iter()
        .map(|x| format!("mainline: {x}"))
        .chain(CIEL_RETRO_ARCHS.iter().map(|x| format!("retro: {x}")))
        .collect::<Vec<_>>();
    let chosen_index = FuzzySelect::with_theme(&theme)
        .with_prompt("Target Architecture")
        .default(default_arch_index)
        .items(prefixed_archs.as_slice())
        .interact()
        .unwrap_or_default();
    Some(all_archs[chosen_index])
}
