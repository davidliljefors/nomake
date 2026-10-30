# nomake

Build tool for C and C++. Write a short `nomake.toml` nomake finds the sources, works out the dependency graph and builds in parallel. Rebuilds only redo what changed.

- Works with clang, clang-cl, cl and gcc
- Builds x64, x86 and ARM64 from one project
- Dependencies are plain folders with their own `nomake.toml`, or libraries from a registry
- Header dependencies are tracked per file

## Install

```
cargo install --path .
```

## Usage

```
nomake new hello
cd hello
nomake run
```

For an existing source tree, run `nomake init` in its root. `nomake configure` edits an existing `nomake.toml`.

| Command | |
| --- | --- |
| `nomake new <name> [--lib\|--shared]` | create a project |
| `nomake init [-y]` | write a `nomake.toml` for the current folder |
| `nomake configure` | edit the existing `nomake.toml` |
| `nomake build` | build |
| `nomake run [-- args]` | build and run the executable |
| `nomake clean` | delete `build/` (keeps `build/vs/`) |
| `nomake compdb` | write `compile_commands.json` |
| `nomake vs` | write a Visual Studio solution to `build/vs/` |
| `nomake add <name>[@version]` | add a registry library and lock it |
| `nomake update [name...]` | move locked libraries to the newest allowed version |
| `nomake list` | show locked libraries |
| `nomake search [text]` | search the registry |
| `nomake info <name>` | show a library's versions, source and features |
| `nomake cache clean` | delete the shared source cache |

| Option | |
| --- | --- |
| `-r`, `--release` | optimized build |
| `-a`, `--arch ARCH` | `x64`, `x86` or `arm64`, default is the host. `vs` takes several |
| `-j`, `--jobs N` | parallel jobs, default is all cores |
| `-v`, `--verbose` | print every command |
| `-d explain` | say why each step is out of date |
| `-d stats` | print how long each phase took |
| `-V`, `--version` | print the version |

`nomake clean -a x86 -r` only removes that configuration.

## Architectures and profiles

Each architecture and profile gets its own folder under `build/`:

```
build/
  x64-debug/
  x64-release/
  arm64-release/
```

MSVC and clang-cl run `vcvarsall.bat` for the requested architecture. clang++ gets `--target`. g++ only does `x86` on an x64 host (`-m32`). Anything else needs a cross compiler set through `CXX`.

## nomake.toml

```toml
[package]
name = "app"
version = "0.1.0"
type = "executable"    # executable | static | shared
cxx_standard = 20

[sources]
dirs = ["src"]    # scanned recursively, src/ by default
files = ["extra/*.cpp"]    # globs
exclude = ["src/experimental/**"]

[include]
public = ["include"]    # visible to dependents
private = ["src"]

[build]
defines = ["FOO=1"]
public_defines = ["USE_FOO"]    # also applied to dependents
flags = ["-Wall"]
link_flags = []
libs = ["user32"]
crt = true    # false builds without the C runtime

[toolchain]
cxx = "clang++"
ar = "llvm-ar"

[dependencies]
mathx = { path = "deps/mathx" }

[[generate]]
name = "embed"
command = ["python", "tools/embed.py", "{input}", "{output}"]
inputs = ["assets/*.bin"]
outputs = ["{stem}.cpp"]
```

Only `[package]` is required. Without `[include]`, `include/` is public and `src/` is private.

Dependencies build first, and their static libraries, system libraries and link flags go into the final link. Headers and `public_defines` are not inherited, a package only sees its direct dependencies' headers. To pass them on, mark the dependency `public = true`.

### Generated files

`[[generate]]` runs `command` once per matched input, or once for all of them with `batch = true` (`{input}` is then the first). Placeholders: `{input}`, `{output}`, `{out_dir}`, `{stem}`. Outputs go to `build/<arch>-<profile>/gen/<package>/`, which is added to the include path, and generated `.cpp` files are compiled. Later steps can use earlier outputs as inputs.

### Platform tables

`[target.<os>]` adds to the base manifest on that OS. Names: `windows`, `macos`, `linux`, `unix` (macOS and Linux). `unix` applies first, then the specific OS, and lists add up.

```toml
[sources]
files = ["src/base/ftbase.c"]

[target.windows]
sources.files = ["builds/windows/ftsystem.c"]

[target.unix]
sources.files = ["builds/unix/ftsystem.c"]

[target.windows.dependencies]
imgui = { public = true }    # merged into the entry with the same name
```

A target table takes `sources`, `include`, `build` and `dependencies`. A dependency that only exists on one OS needs its `path` in the target table. If any table sets `sources` or `include`, the defaults (`src/`, `include/`) are off on every OS.

### Features

A feature adds `sources`, `include` and `build` settings when on. It can't add dependencies.

```toml
[features]
default = ["dx11"]

[feature.dx11]
sources.files = ["backends/imgui_impl_dx11.cpp"]
build.libs = ["d3d11"]

[target.windows.feature.dx11]    # adds to the same feature, on this OS only
build.defines = ["IMGUI_DX11"]
```

```toml
# in a dependent
imgui = { version = "1", features = ["opengl"], default-features = false }
```

Features work on `path` dependencies too. Features requested by different dependents are added up. Defaults stay on unless every dependent sets `default-features = false`. The root project always gets its own defaults.

### Precompiled headers

`[build] pch = "src/pch.h"` precompiles the header once per package and compiles every C++ source against it. Sources keep their normal `#include`, so the code builds the same without `pch`. Works with clang++, cl and clang-cl, not g++.

### Raw flags

`flags` go to the compiler driver. `link_flags` go to the linker (`lld-link` or `link.exe` on Windows with clang or cl, so `/nodefaultlib`, not `-nostdlib`) or to the driver where there is no separate linker. Both come last and can override what nomake adds.

### No C runtime

```toml
[build]
crt = false
entry = "start"    # entry symbol, for executables and shared libraries
subsystem = "console"    # or "windows", link.exe and lld-link only
```

`crt = false` compiles the package freestanding (`-ffreestanding -fno-stack-protector -funwind-tables`, or `/GS- /Zl` and no `/MD` for cl). When that package is linked it also drops the default libraries. Libraries in `libs` are still linked. It's per package, so a static library with `crt = false` can go into a program that uses the runtime.

## Registry

Libraries can also come from a registry, a git repository of recipes at `<name>/<version>/nomake.toml`. A recipe is a normal `nomake.toml` plus a `[source]` table with a git url and a full commit hash. Sources are fetched into a cache shared by all projects. `git` must be on your path.

```
nomake add lua      # writes lua = { version = "5.4" } and locks it
```

A version is exact or a prefix, and the highest match wins. The exact version and commit go into `nomake.lock`, which you should commit. Later builds use the lock, so `"5.4"` stays on `5.4.7` after `5.4.8` is published. `nomake update` moves up to the newest version the requirement allows (`"5.4"` never becomes `5.5`). Only the root project's lock is used. If two packages want different versions of one library, list it in the root `nomake.toml` to choose.

A recipe can offer both builds with `type = ["shared", "static"]` (first is the default) and put DLL-only defines in `[build] shared_defines`. A dependent picks with `link = "static"`; if dependents disagree, the root project wins.

| Variable | |
|---|---|
| `NOMAKE_HOME` | cache folder, default `%LOCALAPPDATA%\nomake` or `~/.cache/nomake` |
| `NOMAKE_REGISTRY` | local registry folder to use instead of the clone, for writing recipes |

## Compiler selection

1. `CXX` environment variable
2. `[toolchain] cxx` in the root `nomake.toml`
3. Windows: `clang++`, then `cl`. Elsewhere: `clang++`, `g++`, then `c++`

`cl` and `clang-cl` get MSVC flags, everything else gets GCC flags.

## Visual Studio

`nomake vs` writes `build/vs/<name>.sln` with Debug and Release for the host architecture. `-a` picks others, comma separated or repeated (`nomake vs -a x64,arm64`). Platforms are named `x64`, `Win32` and `ARM64`. They are NMake projects: Visual Studio browses and edits, build and debug call back into nomake.

## Examples

| Folder | |
| --- | --- |
| [examples/hello](examples/hello) | a single executable |
| [examples/static-lib](examples/static-lib) | an executable using a static library in `deps/` |
| [examples/shared-lib](examples/shared-lib) | an executable using a shared library |
| [examples/options](examples/options) | defines, a language standard and excluding sources |

## License

MIT, see [LICENSE](LICENSE).
