//! Every compiler, archiver and linker switch nomake emits, grouped by dialect. The command-line builders in
//! `toolchain.rs` only walk these tables, so supporting another compiler means adding a table, not another branch.

/// How a switch takes its value.
#[derive(Clone, Copy)]
pub enum Opt {
    /// `-Ifoo`, `/Fo:foo`
    Joined(&'static str),
    /// `-o foo`
    Split(&'static str),
}

impl Opt {
    pub fn push(self, args: &mut Vec<String>, value: impl Into<String>) {
        match self {
            Opt::Joined(flag) => args.push(format!("{flag}{}", value.into())),
            Opt::Split(flag) => args.extend([flag.to_string(), value.into()]),
        }
    }
}

pub fn extend(args: &mut Vec<String>, flags: &[&str]) {
    args.extend(flags.iter().map(|flag| flag.to_string()));
}

pub struct CompileDialect {
    /// Banner suppression, dependency reporting and the like; independent of the source being compiled.
    pub always: &'static [&'static str],
    pub compile_only: &'static str,
    pub object: Opt,
    pub include: Opt,
    pub define: Opt,
    /// Treat the source as C.
    pub c_source: &'static [&'static str],
    /// The C++ standard, given the bare version such as `20`.
    pub std: Opt,
    pub release: &'static [&'static str],
    pub debug: &'static [&'static str],
    /// Selects the (debug) C runtime to link; dropped when building without one.
    pub crt_release: &'static [&'static str],
    pub crt_debug: &'static [&'static str],
    /// No runtime assumptions, no stack cookies or default-library directives.
    pub freestanding: &'static [&'static str],
}

pub const MSVC_COMPILE: CompileDialect = CompileDialect {
    always: &["/nologo", "/showIncludes", "/EHsc"],
    compile_only: "/c",
    object: Opt::Joined("/Fo:"),
    include: Opt::Joined("/I"),
    define: Opt::Joined("/D"),
    c_source: &["/TC"],
    std: Opt::Joined("/std:c++"),
    release: &["/O2", "/DNDEBUG"],
    debug: &["/Od", "/Z7"],
    crt_release: &["/MD"],
    crt_debug: &["/MDd"],
    freestanding: &["/GS-", "/Zl"],
};

pub const GNU_COMPILE: CompileDialect = CompileDialect {
    always: &[],
    compile_only: "-c",
    object: Opt::Split("-o"),
    include: Opt::Joined("-I"),
    define: Opt::Joined("-D"),
    c_source: &["-x", "c"],
    std: Opt::Joined("-std=c++"),
    release: &["-O2", "-DNDEBUG"],
    debug: &["-O0", "-g"],
    crt_release: &[],
    crt_debug: &[],
    // Clang emits no unwind info for C otherwise, which leaves stack traces empty.
    freestanding: &["-ffreestanding", "-fno-stack-protector", "-funwind-tables"],
};

/// Header-dependency output for GNU-style compilers; MSVC reports through /showIncludes.
pub const GNU_DEPFILE: Opt = Opt::Split("-MF");
pub const GNU_DEPFILE_FLAGS: &[&str] = &["-MMD", "-MP"];

/// Objects without a timestamp are byte-identical when nothing changed, which lets the link be skipped.
pub const CL_REPRODUCIBLE: &[&str] = &["/Brepro"];
pub const LLVM_REPRODUCIBLE: &[&str] = &["-mno-incremental-linker-compatible"];
pub const LLVM_REPRODUCIBLE_DEBUG: &[&str] =
    &["-mno-incremental-linker-compatible", "-gcodeview-ghash"];
/// Split DWARF keeps debug info out of the link inputs.
pub const SPLIT_DWARF: &[&str] = &["-gsplit-dwarf"];
pub const PIC: &[&str] = &["-fPIC"];

pub const GNU_PCH_HEADER_LANG: &[&str] = &["-x", "c++-header"];
pub const GNU_PCH_USE: Opt = Opt::Split("-include-pch");
pub const MSVC_PCH_CREATE: Opt = Opt::Joined("/Yc");
pub const MSVC_PCH_USE: Opt = Opt::Joined("/Yu");
pub const MSVC_PCH_FORCE_INCLUDE: Opt = Opt::Joined("/FI");
pub const MSVC_PCH_FILE: Opt = Opt::Joined("/Fp:");

pub const MSVC_ARCHIVE: &[&str] = &["/nologo"];
pub const MSVC_ARCHIVE_OUT: Opt = Opt::Joined("/OUT:");
/// D: no member timestamps or owners, so an identical rebuild is byte-identical (Apple's ar lacks it).
pub const GNU_ARCHIVE: &str = "rcsD";
pub const APPLE_ARCHIVE: &str = "rcs";

pub struct LinkDialect {
    pub out: Opt,
    pub shared: &'static [&'static str],
    /// Import library for a DLL, where the format has them.
    pub implib: Option<Opt>,
    /// Bare name of a system library to the argument that links it.
    pub sys_lib: fn(&str) -> String,
    /// Drops every default library, the C runtime included.
    pub no_default_libs: &'static [&'static str],
    pub entry: Option<Opt>,
    pub subsystem: Option<Opt>,
}

/// link.exe and lld-link, run directly.
pub const COFF_LINK: LinkDialect = LinkDialect {
    out: Opt::Joined("/OUT:"),
    shared: &["/DLL"],
    implib: Some(Opt::Joined("/IMPLIB:")),
    sys_lib: |name| format!("{name}.lib"),
    no_default_libs: &["/NODEFAULTLIB"],
    entry: Some(Opt::Joined("/ENTRY:")),
    subsystem: Some(Opt::Joined("/SUBSYSTEM:")),
};

/// A compiler driver doing the link (gcc, or clang off Windows).
pub const DRIVER_LINK: LinkDialect = LinkDialect {
    out: Opt::Split("-o"),
    shared: &["-shared"],
    implib: None,
    sys_lib: |name| format!("-l{name}"),
    no_default_libs: &["-nostdlib"],
    entry: Some(Opt::Joined("-Wl,-e,")),
    subsystem: None,
};

pub const MSVC_DEBUG_LINK: &str = "/DEBUG";
pub const LLD_DEBUG_LINK: &str = "/DEBUG:GHASH";
pub const DRIVER_DEBUG_LINK: &str = "-g";
pub const LINUX_RPATH_ORIGIN: &str = "-Wl,-rpath,$ORIGIN";

/// Prefix of the default-library switches a compiler driver bakes into the linker command it would run.
pub fn is_default_lib(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    ["-defaultlib:", "/defaultlib:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}
