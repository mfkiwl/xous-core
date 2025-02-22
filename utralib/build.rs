use std::path::PathBuf;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;

fn out_dir() -> PathBuf {
    PathBuf::from(env::var_os("OUT_DIR").unwrap())
}

/// Helper macro that returns a constant number of features enabled among specified list.
macro_rules! count_enabled_features {
    ($($feature:literal),*) => {
        {
            let mut enabled_features = 0;
            $(
                enabled_features += cfg!(feature = $feature) as u32;
            )*
            enabled_features
        }
    }
}

/// Helper macro that returns a compile-time error if multiple or none of the
/// features of some caterogy are defined.
///
/// # Example
///
/// Given the following code:
///
/// ```
/// allow_single_feature!("feature-category-name", "a", "b", "c");
/// ```
///
/// These runs fail compilation check:
/// $ cargo check --features a,b # error msg: 'Multiple feature-category-name specified. Only one is allowed.
/// $ cargo check # error msg: 'None of the feature-category-name specified. Pick one.'
///
/// This compiles:
/// $ cargo check --feature a
macro_rules! allow_single_feature {
    ($name:literal, $($feature:literal),*) => {
        const _: () = {
            const MSG_MULTIPLE: &str = concat!("\nMultiple ", $name, " specified. Only one is allowed.");
            const MSG_NONE: &str = concat!("\nNone of the ", $name, " specified. Pick one.");

            match count_enabled_features!($($feature),*) {
                0 => std::panic!("{}", MSG_NONE),
                1 => {}
                2.. => std::panic!("{}", MSG_MULTIPLE),
            }
        };
    }
}

macro_rules! allow_single_target_feature {
    ($($args:tt)+) => {
        allow_single_feature!("targets", $($args)+);
    }
}

#[cfg(feature = "precursor")] // Gitrevs are only relevant for Precursor target
macro_rules! allow_single_gitrev_feature {
    ($($args:tt)+) => {
        allow_single_feature!("gitrevs", $($args)+);
    }
}

fn main() {
    // ------ check that the feature flags are sane -----

    allow_single_target_feature!("precursor", "hosted", "renode");

    #[cfg(feature="precursor")]
    allow_single_gitrev_feature!("precursor-c809403", "precursor-c809403-perflib", "precursor-2753c12-dvt");

    // ----- select an SVD file based on a specific revision -----
    #[cfg(feature="precursor-c809403")]
    let svd_filename = "precursor/soc-c809403.svd";
    #[cfg(feature="precursor-c809403")]
    let generated_filename = "src/generated/precursor_c809403.rs";

    #[cfg(feature="precursor-c809403-perflib")]
    let svd_filename = "precursor/soc-perf-c809403.svd";
    #[cfg(feature="precursor-c809403-perflib")]
    let generated_filename = "src/generated/precursor_perf_c809403.rs";

    #[cfg(feature="renode")]
    let svd_filename = "renode/renode.svd";
    #[cfg(feature="renode")]
    let generated_filename = "src/generated/renode.rs";

    #[cfg(feature="precursor-2753c12-dvt")]
    let svd_filename = "precursor/soc-dvt-2753c12.svd";
    #[cfg(feature="precursor-2753c12-dvt")]
    let generated_filename = "src/generated/precursor_dvt_2753c12.rs";

    // ----- control file generation and rebuild sequence -----
    // check and see if the configuration has changed since the last build. This should be
    // passed by the build system (e.g. xtask) if the feature is used.
    #[cfg(not(feature="hosted"))]
    {
        let last_config = out_dir().join("../../LAST_CONFIG");
        if last_config.exists() {
            println!("cargo:rerun-if-changed={}", last_config.canonicalize().unwrap().display());
        }
        let svd_file_path = std::path::Path::new(&svd_filename);
        println!("cargo:rerun-if-changed={}", svd_file_path.canonicalize().unwrap().display());

        let src_file = std::fs::File::open(svd_filename).expect("couldn't open src file");
        let mut dest_file = std::fs::File::create(generated_filename).expect("couldn't open dest file");
        svd2utra::generate(src_file, &mut dest_file).unwrap();

        // ----- feedback SVD path to build framework -----
        // pass the computed SVD filename back to the build system, so that we can pass this
        // on to the image creation program. This is necessary so we can extract all the memory
        // regions and create the whitelist of memory pages allowed to the kernel; any page not
        // explicitly used by the hardware model is ineligible for mapping and allocation by any
        // process. This helps to prevent memory aliasing attacks by hardware blocks that partially
        // decode their addresses (this would be in anticipation of potential hardware bugs; ideally
        // this isn't ever a problem).
        let svd_path = out_dir().join("../../SVD_PATH");
        let mut svd_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(svd_path)
            .unwrap();
        write!(svd_file, "utralib/{}", svd_filename).unwrap();
    }
    #[cfg(feature="hosted")]
    {
        let svd_path = out_dir().join("../../SVD_PATH");
        let mut svd_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(svd_path)
            .unwrap();
        write!(svd_file, "").unwrap(); // there is no SVD file for hosted mode
    }
}
