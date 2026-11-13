# nomake

C/C++ build tool. Write a small `nomake.toml` and `nomake` scans sources, resolves dependencies, and builds in parallel.

- Supports `clang`, `clang-cl`, `cl`, `gcc`, and `em++` (Emscripten)
- Targets `x64`, `x86`, `arm64`, and `wasm32`
- Folder and registry dependencies with lockfile (`nomake.lock`)
- Incremental rebuilds with header dependency tracking

## Install

```
cargo install --path .
```

## Quick start

```
nomake new hello
cd hello
nomake run
```

For an existing project, run `nomake init`.

| Command | Description |
| --- | --- |
| `nomake new <name> [--lib\|--shared]` | Create a new project |
| `nomake init [-y]` | Generate `nomake.toml` for the current directory |
| `nomake configure` | Interactive project and dependency editor |
| `nomake build` | Build project |
| `nomake run [-- args]` | Build and run executable |
| `nomake test [-- args]` | Build and run `type = "test"` packages |
| `nomake clean` | Remove `build/` (keeps `build/vs/`) |
| `nomake compdb` | Write `compile_commands.json` |
| `nomake vs` | Write a Visual Studio solution to `build/vs/` |
| `nomake add <name>[@version]` | Add and lock a registry dependency |
| `nomake update [name...]` | Update locked dependencies |
| `nomake list` | List locked dependencies |
| `nomake search [text]` | Search the registry |
| `nomake info <name>` | Show package versions, source, and features |
| `nomake cache clean` | Remove cached dependency sources |

| Option | Description |
| --- | --- |
| `-r`, `--release` | Release build |
| `-a`, `--arch ARCH` | `x64`, `x86`, `arm64`, or `wasm32` (default: host; `vs` accepts a comma list) |
| `-j`, `--jobs N` | Parallel jobs (default: all cores) |
| `-v`, `--verbose` | Print executed commands |
| `-d explain` | Print why targets rebuild |
| `-d stats` | Print phase timing |
| `-V`, `--version` | Print version |

```
nomake build -r -a arm64    # outputs to build/arm64-release/
nomake clean -a x86 -r      # removes only build/x86-release/
```

## `nomake.toml`

Only `[package]` is required.

```toml
[package]
name = "app"
version = "0.1.0"
type = "executable"             # executable | static | shared | test
cxx_standard = 20

[sources]
dirs = ["src"]                  # recursive; defaults to ["src"]
files = ["extra/*.cpp"]         # globs
exclude = ["src/experimental/**"]

[include]
public = ["include"]            # exported to dependents
private = ["src"]               # defaults: include/ public, src/ private

[build]
defines = ["FOO=1"]
public_defines = ["USE_FOO"]    # exported to dependents
flags = ["-Wall"]               # appended to compile commands
link_flags = []                 # appended to link commands
libs = ["user32"]
crt = "dynamic"                 # dynamic (default) | static
subsystem = "console"           # console | windows (MSVC/lld-link)
pch = "src/pch.h"               # precompiled header (clang++, cl, clang-cl)

[toolchain]
cxx = "clang++"
ar = "llvm-ar"

[dependencies]
mathx = { path = "deps/mathx" }
```

Mark a dependency `public = true` to re-export its `include.public` and `public_defines`.

### Code generation

```toml
[[generate]]
name = "embed"
command = ["python", "tools/embed.py", "{input}", "{output}"]
inputs = ["assets/*.bin"]
outputs = ["{stem}.cpp"]
```

Runs once per input (or once for all inputs with `batch = true`). Placeholders: `{input}`, `{output}`, `{out_dir}`, `{stem}`, `{depfile}`. Outputs are written to `build/<arch>-<profile>/gen/<package>/` (added to the include path), and generated `.c`/`.cpp` files are compiled automatically.

To run an executable dependency as a generator tool:

```toml
[dependencies]
my-gen = { path = "../my-gen" }

[[generate]]
tool = "my-gen"
inputs = ["schema/*.in"]
outputs = ["{stem}.gen.hpp", "{stem}.gen.cpp"]
```

`nomake` invokes `tool --out-dir <gen_dir> --depfile <file> -I<dir>... <input>`.

### Target tables

`[target.<os>]` merges OS-specific settings (`sources`, `include`, `build`, `dependencies`, `feature`, `generate`) into the manifest. Supported targets: `windows`, `macos`, `linux`, `emscripten`, and `unix` (all non-Windows targets, including `emscripten`).

```toml
[sources]
files = ["src/base/ftbase.c"]

[target.windows]
sources.files = ["builds/windows/ftsystem.c"]

[target.unix]
sources.files = ["builds/unix/ftsystem.c"]

[target.windows.dependencies]
imgui = { public = true }
```

### Features

```toml
[features]
default = ["dx11"]

[feature.dx11]
sources.files = ["backends/imgui_impl_dx11.cpp"]
build.libs = ["d3d11"]
peer = ["sdl3"]                 # use headers/defines from root's sdl3 dependency

[target.windows.feature.dx11]
build.defines = ["IMGUI_DX11"]
```

```toml
[dependencies]
imgui = { version = "1", features = ["opengl"], default-features = false }
```

## Registry

Registry packages live at `<name>/<version>/nomake.toml` with a `[source]` table (`git` URL and 40-char commit `rev`). Resolved versions are recorded in `nomake.lock` and sources are cached under `NOMAKE_HOME` (`%LOCALAPPDATA%\nomake` or `~/.cache/nomake`).

```
nomake add lua      # adds lua = { version = "5.4" } and writes nomake.lock
nomake update lua   # updates within 5.4.x
```

Libraries that support both linkage modes set `type = ["shared", "static"]` (with optional `[build] shared_defines`); dependents can select `link = "static"` or `link = "shared"`.

| Environment variable | Description |
| --- | --- |
| `NOMAKE_HOME` | Cache directory (`%LOCALAPPDATA%\nomake` or `~/.cache/nomake`) |
| `NOMAKE_REGISTRY` | Local registry path override |

## Compiler selection

1. `CXX` environment variable
2. `[toolchain] cxx` in the root `nomake.toml`
3. Default per target — `wasm32`: `em++`; Windows: `clang++`, `cl`, `g++`; Unix: `clang++`, `g++`, `c++`

## Emscripten (`-a wasm32`)

Activate `emsdk` so `em++` and `emar` are on `PATH`, then build with `-a wasm32`:

```
nomake build -a wasm32
nomake run -a wasm32
nomake test -a wasm32
```

Executables output `build/wasm32-<profile>/<name>.html` alongside `<name>.js` and `<name>.wasm` (plus `<name>.data` when `--preload-file` is used). `nomake run -a wasm32` and `nomake test -a wasm32` execute `<name>.js` with `node`; serve `<name>.html` over HTTP for browser apps.

```toml
[target.emscripten.build]
link_flags = ["--embed-file", "assets", "-sALLOW_MEMORY_GROWTH=1"]
```

Files passed to `--shell-file`, `--pre-js`, `--post-js`, `--js-library`, `--embed-file`, and `--preload-file` are tracked as link inputs so edits trigger relinking. Shared libraries are not supported on Emscripten; use `type = "static"` or `link = "static"`.

## Visual Studio & Workspaces

```
nomake vs
nomake vs -a x64,arm64
```

Generates an NMake-based solution in `build/vs/<name>.sln` that delegates build/clean to `nomake`.

Multi-package repositories can define `[workspace]` in the root `nomake.toml`:

```toml
[workspace]
members = ["my-lib", "my-lib/tests", "tools/*"]
```

```
nomake build                # build all non-test members in a single graph
nomake build app tests      # build specific members
nomake run app              # run a member (defaults to first executable)
nomake test                 # build and run all test members
```

## Examples

| Path | Description |
| --- | --- |
| [examples/hello](examples/hello) | Single executable |
| [examples/static-lib](examples/static-lib) | Executable with a static library in `deps/` |
| [examples/shared-lib](examples/shared-lib) | Executable with a shared library |
| [examples/options](examples/options) | Defines, C++ standard, and source exclusions |
| [examples/emscripten](examples/emscripten) | Native + WebAssembly executable with an embedded file |

## License

MIT — see [LICENSE](LICENSE).
