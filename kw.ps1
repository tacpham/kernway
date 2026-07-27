# kw.ps1 — Kernway build tool
# No Rust install needed. Just Docker.
#
# Usage:
#   .\kw.ps1 build          — build workspace
#   .\kw.ps1 test           — run tests
#   .\kw.ps1 run hello-di   — run an example
#   .\kw.ps1 run            — run the current crate
#   .\kw.ps1 new my-api     — create a new Kernway project
#   .\kw.ps1 check          — fast type-check
#   .\kw.ps1 clippy         — lint
#   .\kw.ps1 fmt            — format code
#   .\kw.ps1 shell          — open bash inside the container

param(
    [Parameter(Position=0)] [string] $Command = "help",
    [Parameter(Position=1)] [string] $Arg = ""
)

# Docker image — official, no custom image needed. Must be >= 1.85: the dep tree
# (zeroize 1.9, via ring/rustls) needs edition2024, which older Cargo rejects.
# The `1` tag tracks the newest 1.x. Kept in sync with kw.sh.
$IMAGE = "rust:1-bookworm"

# ============================================================
# Corporate SSL proxy — export Windows CA certs into the container
# ============================================================
$CERT_FILE = "$env:TEMP\kernway-ca-bundle.pem"

function Export-WindowsCerts {
    if (Test-Path $CERT_FILE) { return }
    Write-Host "  → Exporting Windows CA certs for corporate proxy..." -ForegroundColor DarkGray
    $certs = Get-ChildItem -Path Cert:\LocalMachine\Root -ErrorAction SilentlyContinue
    $pem = [System.Collections.Generic.List[string]]::new()
    foreach ($cert in $certs) {
        try {
            $b64 = [Convert]::ToBase64String($cert.RawData)
            $pem.Add("-----BEGIN CERTIFICATE-----")
            for ($i = 0; $i -lt $b64.Length; $i += 64) {
                $pem.Add($b64.Substring($i, [Math]::Min(64, $b64.Length - $i)))
            }
            $pem.Add("-----END CERTIFICATE-----")
        } catch {}
    }
    # Unix line endings (LF) — required for the Linux container
    [System.IO.File]::WriteAllText($CERT_FILE, ($pem -join "`n") + "`n", [System.Text.Encoding]::ASCII)
    Write-Host "  → Exported $($certs.Count) certs to $CERT_FILE" -ForegroundColor DarkGray
}

# Volume caches — persisted between runs. Registry avoids re-downloading deps;
# target/ lives in its own volume (not the host bind-mount) so the container
# build is incremental and isolated from any host-native target/. Kept in sync
# with kw.sh.
$CACHE_VOL  = "kernway-cargo-cache"
$TARGET_VOL = "kernway-target"

# Base docker run command
function Invoke-Docker {
    param([string[]] $CargoArgs)
    Export-WindowsCerts
    docker run --rm `
        -v "${PWD}:/workspace" `
        -v "${CACHE_VOL}:/usr/local/cargo/registry" `
        -v "${TARGET_VOL}:/workspace/target" `
        -v "${CERT_FILE}:/tmp/corp-ca.pem" `
        -e SSL_CERT_FILE=/tmp/corp-ca.pem `
        -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem `
        -e CURL_CA_BUNDLE=/tmp/corp-ca.pem `
        -w /workspace `
        $IMAGE `
        cargo @CargoArgs
}

# Like Invoke-Docker, but adds clippy/rustfmt first (the base image ships only a
# shim, not the components). Used by the lint/format commands.
function Invoke-DockerTooled {
    param([string[]] $CargoArgs)
    Export-WindowsCerts
    docker run --rm `
        -v "${PWD}:/workspace" `
        -v "${CACHE_VOL}:/usr/local/cargo/registry" `
        -v "${TARGET_VOL}:/workspace/target" `
        -v "${CERT_FILE}:/tmp/corp-ca.pem" `
        -e SSL_CERT_FILE=/tmp/corp-ca.pem `
        -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem `
        -e CURL_CA_BUNDLE=/tmp/corp-ca.pem `
        -w /workspace `
        $IMAGE `
        bash -c 'rustup component add clippy rustfmt >/dev/null 2>&1 || true; exec cargo "$@"' _ @CargoArgs
}

switch ($Command) {
    "build" {
        Write-Host ">> cargo build --workspace" -ForegroundColor Cyan
        Invoke-Docker "build", "--workspace"
    }

    "release" {
        Write-Host ">> cargo build --workspace --release" -ForegroundColor Cyan
        Invoke-Docker "build", "--workspace", "--release"
    }

    "test" {
        Write-Host ">> cargo test --workspace" -ForegroundColor Cyan
        Invoke-Docker "test", "--workspace"
    }

    "check" {
        Write-Host ">> cargo check --workspace" -ForegroundColor Cyan
        Invoke-Docker "check", "--workspace"
    }

    "clippy" {
        Write-Host ">> cargo clippy --workspace" -ForegroundColor Cyan
        Invoke-DockerTooled "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"
    }

    "fmt" {
        Write-Host ">> cargo fmt --all" -ForegroundColor Cyan
        Invoke-DockerTooled "fmt", "--all"
    }

    "run" {
        Export-WindowsCerts
        if ($Arg) {
            Write-Host ">> cargo run -p $Arg" -ForegroundColor Cyan
            docker run --rm `
                -p 8080:8080 `
                -v "${PWD}:/workspace" `
                -v "${CACHE_VOL}:/usr/local/cargo/registry" `
                -v "${TARGET_VOL}:/workspace/target" `
                -v "${CERT_FILE}:/tmp/corp-ca.pem" `
                -e SSL_CERT_FILE=/tmp/corp-ca.pem `
                -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem `
                -e CURL_CA_BUNDLE=/tmp/corp-ca.pem `
                -w /workspace `
                $IMAGE `
                cargo run -p $Arg
        } else {
            Write-Host ">> cargo run" -ForegroundColor Cyan
            docker run --rm `
                -p 8080:8080 `
                -v "${PWD}:/workspace" `
                -v "${CACHE_VOL}:/usr/local/cargo/registry" `
                -v "${TARGET_VOL}:/workspace/target" `
                -v "${CERT_FILE}:/tmp/corp-ca.pem" `
                -e SSL_CERT_FILE=/tmp/corp-ca.pem `
                -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem `
                -e CURL_CA_BUNDLE=/tmp/corp-ca.pem `
                -w /workspace `
                $IMAGE `
                cargo run
        }
    }

    "new" {
        $ProjectName = $Arg
        if (-not $ProjectName) {
            Write-Error "Usage: .\kw.ps1 new <project-name>"
            exit 1
        }

        $ProjectDir = Join-Path (Get-Location) $ProjectName
        if (Test-Path $ProjectDir) {
            Write-Error "Directory '$ProjectName' already exists"
            exit 1
        }

        New-Item -ItemType Directory -Path $ProjectDir | Out-Null
        New-Item -ItemType Directory -Path "$ProjectDir\src" | Out-Null

        @"
[package]
name    = "$ProjectName"
version = "0.1.0"
edition = "2021"

[dependencies]
kernway        = "0.1"
kernway-server = "0.1"
kernway-web    = "0.1"
di-core        = "0.1"
di-macro       = "0.1"
serde          = { version = "1", features = ["derive"] }
serde_json     = "1"
"@ | Set-Content "$ProjectDir\Cargo.toml" -Encoding UTF8

        @"
//! $ProjectName — Kernway application
//!
//! Generated by: .\kw.ps1 new $ProjectName
//! Run with:     ..\kw.ps1 run

use di_core::AppContext;
use di_macro::Component;
use kernway::response::IntoResponse;
use kernway_server::{
    middleware::{LoggingMiddleware, RequestIdMiddleware},
    KernwayApp,
};
use kernway_web::Json;

#[derive(Component)]
pub struct GreetingService;

impl GreetingService {
    pub fn hello(&self, name: &str) -> String {
        format!("Hello, {}! Welcome to Kernway.", name)
    }
}

fn main() {
    let mut ctx = AppContext::new();
    ctx.build::<GreetingService>().unwrap();

    println!("✅ {} beans registered", ctx.bean_count());

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(RequestIdMiddleware)
        .layer(LoggingMiddleware)
        .get("/health", |_req, _ctx| {
            Json(serde_json::json!({"status": "UP", "app": "$ProjectName"})).into_response()
        })
        .get("/hello/{name}", |req, ctx| {
            let name = req.path_params.get("name").map(|s| s.as_str()).unwrap_or("World");
            let svc = ctx.get::<GreetingService>().unwrap();
            Json(serde_json::json!({"message": svc.hello(name)})).into_response()
        })
        .build()
        .run();
}
"@ | Set-Content "$ProjectDir\src\main.rs" -Encoding UTF8

        @"
/target
Cargo.lock
"@ | Set-Content "$ProjectDir\.gitignore" -Encoding UTF8

        @"
# $ProjectName

A Kernway web application.

## Run

```powershell
..\kw.ps1 run
```

## Test

```powershell
curl http://localhost:8080/health
curl http://localhost:8080/hello/World
```
"@ | Set-Content "$ProjectDir\README.md" -Encoding UTF8

        Write-Host ""
        Write-Host "✅ Created Kernway project: $ProjectName" -ForegroundColor Green
        Write-Host ""
        Write-Host "  Next steps:" -ForegroundColor Cyan
        Write-Host "    cd $ProjectName"
        Write-Host "    ..\kw.ps1 run"
        Write-Host ""
    }

    "shell" {
        Write-Host ">> bash (Ctrl+D to exit)" -ForegroundColor Cyan
        Export-WindowsCerts
        docker run --rm -it `
            -v "${PWD}:/workspace" `
            -v "${CACHE_VOL}:/usr/local/cargo/registry" `
            -v "${TARGET_VOL}:/workspace/target" `
            -v "${CERT_FILE}:/tmp/corp-ca.pem" `
            -e SSL_CERT_FILE=/tmp/corp-ca.pem `
            -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem `
            -e CURL_CA_BUNDLE=/tmp/corp-ca.pem `
            -w /workspace `
            $IMAGE `
            bash
    }

    "clean-cache" {
        Write-Host ">> Removing cached volumes ($CACHE_VOL, $TARGET_VOL)" -ForegroundColor Yellow
        docker volume rm $CACHE_VOL $TARGET_VOL
    }

    default {
        Write-Host ""
        Write-Host "  Kernway Build Tool (no Rust install needed)" -ForegroundColor Green
        Write-Host ""
        Write-Host "  .\kw.ps1 build              Build workspace"
        Write-Host "  .\kw.ps1 release            Build release"
        Write-Host "  .\kw.ps1 test               Run tests"
        Write-Host "  .\kw.ps1 check              Fast type-check"
        Write-Host "  .\kw.ps1 clippy             Lint"
        Write-Host "  .\kw.ps1 fmt                Format code"
        Write-Host "  .\kw.ps1 run [name]         Run an example or the current crate"
        Write-Host "  .\kw.ps1 new my-api         Create a new Kernway project"
        Write-Host "  .\kw.ps1 shell              Open a bash container"
        Write-Host "  .\kw.ps1 clean-cache        Remove cargo cache"
        Write-Host ""
    }
}
