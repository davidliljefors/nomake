# nomake

Build tool for C and C++. Write a short `nomake.toml` and nomake finds the sources, works out the dependency graph and builds in parallel.

- Works with clang, clang-cl, cl and gcc
- Builds x64, x86 and ARM64 from one project
- Dependencies are plain folders or libraries from a registry
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

For an existing source tree, run `nomake init` in its root.

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

```
nomake build -r -a arm64    # build/arm64-release/
nomake clean -a x86 -r      # only removes x86-release
```

Each architecture and profile gets its own folder under `build/`. MSVC and clang-cl run `vcvarsall.bat` for the architecture, clang++ gets `--target`. g++ only does `x86` on an x64 host. For anything else set a cross compiler with `CXX`.

## nomake.toml

Only `[package]` is required.

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
private = ["src"]       # defaults: include/ public, src/ private

[build]
defines = ["FOO=1"]
public_defines = ["USE_FOO"]    # also applied to dependents
flags = ["-Wall"]               # compiler driver, last on the command line
link_flags = []                 # linker, last on the command line
libs = ["user32"]
crt = "dynamic"                 # or "static", one value for the whole build
subsystem = "console"           # or "windows", link.exe and lld-link only
pch = "src/pch.h"               # clang++, cl and clang-cl

[toolchain]
cxx = "clang++"
ar = "llvm-ar"

[dependencies]
mathx = { path = "deps/mathx" }
```

Dependencies build first and their libraries and link flags go into the final link. Headers and `public_defines` are not inherited. Mark a dependency `public = true` to pass them on.

### Generated files

```toml
[[generate]]
name = "embed"
command = ["python", "tools/embed.py", "{input}", "{output}"]
inputs = ["assets/*.bin"]
outputs = ["{stem}.cpp"]
```

Runs once per input, or once for all with `batch = true`. Placeholders are `{input}`, `{output}`, `{out_dir}`, `{stem}` and `{depfile}`. Outputs go to `build/<arch>-<profile>/gen/<package>/`, which is on the include path. Generated `.cpp` files are compiled.

A step can use an executable package as a `tool` instead of a `command`:

```toml
[dependencies]
my-gen = { path = "../my-gen" }

[[generate]]
tool = "my-gen"
inputs = ["schema/*.in"]
outputs = ["{stem}.gen.hpp", "{stem}.gen.cpp"]
```

It is called as `tool --out-dir <gen dir> --depfile <file> -I<dir>... <input>` and must write a Makefile-style depfile (`out: dep dep ...`) listing other files it read. With `command`, add `{depfile}` to the arguments to opt in.

### Platform tables

`[target.<os>]` adds to the base manifest. Names are `windows`, `macos`, `linux` and `unix` (macOS and Linux).

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

Tables take `sources`, `include`, `build` and `dependencies`. If any table sets `sources` or `include`, the defaults are off on every OS.

### Features

```toml
[features]
default = ["dx11"]

[feature.dx11]
sources.files = ["backends/imgui_impl_dx11.cpp"]
build.libs = ["d3d11"]
peer = ["sdl3"]    # use a root dependency's headers and defines, no linking

[target.windows.feature.dx11]    # same feature, this OS only
build.defines = ["IMGUI_DX11"]
```

```toml
# in a dependent
imgui = { version = "1", features = ["opengl"], default-features = false }
```

Features work on `path` dependencies too. Requests from different dependents are added up.

### C runtime

On Windows `dynamic` is `/MD` (`/MDd` in debug) and `static` is `/MT`. On Linux `static` links with `-static`. Every package in a build must agree on `crt`.

## Registry

A registry is a git repository of recipes at `<name>/<version>/nomake.toml`. A recipe is a normal `nomake.toml` plus a `[source]` table with a git url and a full commit hash. Sources are cached and shared by all projects.

```
nomake add lua      # writes lua = { version = "5.4" } and locks it
nomake update lua   # newest 5.4.x, never 5.5
```

Commit `nomake.lock`. Only the root project's lock is used, so list a library in the root `nomake.toml` to pick its version.

A recipe can offer both builds with `type = ["shared", "static"]` (first is the default) and `[build] shared_defines` for DLL-only defines. A dependent picks with `link = "static"`.

| Variable | |
|---|---|
| `NOMAKE_HOME` | cache folder, default `%LOCALAPPDATA%\nomake` or `~/.cache/nomake` |
| `NOMAKE_REGISTRY` | local registry folder, for writing recipes |

## Compiler selection

1. `CXX` environment variable
2. `[toolchain] cxx` in the root `nomake.toml`
3. Windows: `clang++`, then `cl`. Elsewhere: `clang++`, `g++`, then `c++`

## Visual Studio

```
nomake vs
nomake vs -a x64,arm64
```

Writes `build/vs/<name>.sln` with Debug and Release. They are NMake projects, so build and debug call back into nomake.

## Workspaces

```toml
[workspace]
members = ["my-lib", "my-lib/tests", "tools/*"]
```

```
nomake build                # every member
nomake build app tests      # some members
nomake run app
```

## Examples

| Folder | |
| --- | --- |
| [examples/hello](examples/hello) | a single executable |
| [examples/static-lib](examples/static-lib) | an executable using a static library in `deps/` |
| [examples/shared-lib](examples/shared-lib) | an executable using a shared library |
| [examples/options](examples/options) | defines, a language standard and excluding sources |

## License

MIT, see [LICENSE](LICENSE).
