# cresset/magebuild

[magebuild](https://github.com/cresset-tools/magebuild) — a fast, parallel,
DAG-ordered Magento 2 build orchestrator. It runs `composer install`,
`di-compile`, `static-content deploy`, `autoload dump`, and packaging in one
process, with each step a linked in-process engine call, replacing the build
half of a Deployer recipe.

```bash
composer require --dev cresset/magebuild
vendor/bin/magebuild --root . --artifact release.tar.zst --deployed-version "$(date +%s)"
vendor/bin/magebuild plan
```

Or install it globally:

```bash
composer global require cresset/magebuild
```

magebuild is a single Rust binary. This package ships **only a thin PHP
launcher** — no Rust source. On first run it downloads the prebuilt
`magebuild` binary matching this package's version for your platform, caches
it (`$XDG_CACHE_HOME/magebuild/<version>/`), verifies its SHA-256, and execs
it. The package version maps 1:1 to the magebuild release:
`cresset/magebuild:0.1.0` runs `magebuild-v0.1.0`.

Prebuilt targets: Linux x86_64 (gnu/musl), Linux arm64 (gnu), macOS arm64/x64,
Windows x64. `ext-curl` is recommended; `ext-zip` is required on Windows.

The download layout is also declared machine-readably in composer.json under
`extra.bougie.native-binary` (spec 1 = the cargo-dist release layout). Tools
that install this package — [bougie](https://github.com/cresset-tools/bougie)'s
`bougie tool install cresset/magebuild` — use it to prefetch and
SHA-256-verify the binary into the same cache the launcher probes, so the tool
works immediately (and offline) instead of downloading on first run. Composer
itself ignores the block. It must stay in sync with `src/Launcher.php` and
with the repo's dist-workspace.toml target list.

Release archives are additionally signed with Sigstore (cosign keyless via
GitHub Actions OIDC; `<archive>.sig` bundle sidecars, logged in the Rekor
transparency log). The `sigstore` key in that block pins this repository as
the signing identity, and bougie verifies it fail-closed before caching a
prefetched binary — the composer tag is only published after signing
completes, so every Packagist version has signed binaries. The PHP launcher
itself verifies the SHA-256 sidecar only.

This is the Composer distribution branch of the magebuild repo — it is
generated from `packaging/composer/` on `main` and contains no application
code of its own. EUPL-1.2.
