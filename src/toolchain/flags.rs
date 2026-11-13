use crate::config::Crt;

#[derive(Clone, Copy)]
pub enum Opt {
    Joined(&'static str),
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
    pub always: &'static [&'static str],
    pub compile_only: &'static str,
    pub object: Opt,
    pub include: Opt,
    pub define: Opt,
    pub c_source: &'static [&'static str],
    pub std: Opt,
    pub release: &'static [&'static str],
    pub debug: &'static [&'static str],
    pub crt: [&'static [&'static str]; 4],
}

pub fn crt_index(crt: Crt, release: bool) -> usize {
    match (crt, release) {
        (Crt::Dynamic, true) => 0,
        (Crt::Dynamic, false) => 1,
        (Crt::Static, true) => 2,
        (Crt::Static, false) => 3,
    }
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
    crt: [&["/MD"], &["/MDd"], &["/MT"], &["/MTd"]],
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
    crt: [&[], &[], &[], &[]],
};

pub const CLANG_WINDOWS_CRT: [&[&str]; 4] = [
    &["-fms-runtime-lib=dll"],
    &["-fms-runtime-lib=dll_dbg"],
    &["-fms-runtime-lib=static"],
    &["-fms-runtime-lib=static_dbg"],
];

pub const GNU_DEPFILE: Opt = Opt::Split("-MF");
pub const GNU_DEPFILE_FLAGS: &[&str] = &["-MMD", "-MP"];

pub const CODEVIEW_GHASH: &[&str] = &["-gcodeview-ghash"];
pub const PIC: &[&str] = &["-fPIC"];

pub const GNU_PCH_HEADER_LANG: &[&str] = &["-x", "c++-header"];
pub const GNU_PCH_USE: Opt = Opt::Split("-include-pch");
pub const MSVC_PCH_CREATE: Opt = Opt::Joined("/Yc");
pub const MSVC_PCH_USE: Opt = Opt::Joined("/Yu");
pub const MSVC_PCH_FORCE_INCLUDE: Opt = Opt::Joined("/FI");
pub const MSVC_PCH_FILE: Opt = Opt::Joined("/Fp:");

pub const MSVC_ARCHIVE: &[&str] = &["/nologo"];
pub const MSVC_ARCHIVE_OUT: Opt = Opt::Joined("/OUT:");

pub struct LinkDialect {
    pub out: Opt,
    pub shared: &'static [&'static str],
    pub implib: Option<Opt>,
    pub sys_lib: fn(&str) -> String,
    pub subsystem: Option<Opt>,
}

pub const COFF_LINK: LinkDialect = LinkDialect {
    out: Opt::Joined("/OUT:"),
    shared: &["/DLL"],
    implib: Some(Opt::Joined("/IMPLIB:")),
    sys_lib: |name| format!("{name}.lib"),
    subsystem: Some(Opt::Joined("/SUBSYSTEM:")),
};

pub const DRIVER_LINK: LinkDialect = LinkDialect {
    out: Opt::Split("-o"),
    shared: &["-shared"],
    implib: None,
    sys_lib: |name| format!("-l{name}"),
    subsystem: None,
};

pub const STATIC_LINK: &[&str] = &["-static"];

pub const MSVC_DEBUG_LINK: &str = "/DEBUG";
pub const LLD_DEBUG_LINK: &str = "/DEBUG:GHASH";
pub const DRIVER_DEBUG_LINK: &str = "-g";
pub const LINUX_RPATH_ORIGIN: &str = "-Wl,-rpath,$ORIGIN";

/// Strip default CRT flags from probed linker args so object files control CRT selection.
pub fn is_runtime_lib(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    ["-defaultlib:", "/defaultlib:"].iter().any(|prefix| {
        lower.strip_prefix(prefix).is_some_and(|name| {
            matches!(
                name.trim_end_matches(".lib"),
                "libcmt" | "libcmtd" | "msvcrt" | "msvcrtd"
            )
        })
    })
}
