# magebuild

Build a Magento 2 release as fast as possible: a parallel, DAG-ordered
orchestrator that runs `composer install`, DI compile, static-content deploy,
autoload dump, and packaging in **one process**, replacing the build half of a
Deployer recipe.

magebuild links [magequery](https://github.com/cresset-tools/magequery)'s
write-side engines in-process (`magecommand-engine` for the DI compile,
magecommand's `static_deploy` for static content) together with a native
Composer client, so a full build needs no PHP and no framework bootstrap. The
steps run on a bounded parallel ready-queue over a shared rayon pool, with DI
compile and static-content deploy overlapped:

```
composer-install → { di-compile → autoload-dump } ∥ { static-deploy } → package
```

## Install

### curl (Linux / macOS)

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/cresset-tools/magebuild/releases/latest/download/magebuild-installer.sh | sh
```

Prebuilt binaries ship for Linux (gnu and musl, x86_64 and arm64), macOS
(x86_64 and arm64), and Windows. Every release archive is Sigstore-signed.

### Composer (for Deployer / CI projects)

```sh
composer require --dev cresset/magebuild
```

This puts `magebuild` in `vendor/bin/`; the package fetches the prebuilt binary
for your platform on first run (checksum-verified, cached).

## Usage

```sh
magebuild                              # build the Magento root in the current dir
magebuild --root path/to/store         # ... or an explicit root
magebuild --artifact release.tar.zst   # also package (gzip/zstd chosen by extension)
magebuild plan                         # print the DAG + parallel schedule, run nothing
magebuild node di-compile              # run one node and its dependencies
magebuild --json                       # machine-readable output
```

Common flags: `--jobs N`, `--only <ids>`, `--skip <ids>`, `--dry-run`,
`--profile`, `--deployed-version <v>`, `--exclude-from <file>`.

Packaging picks the compressor from the artifact extension: `.tar` (none),
`.tar.gz` (parallel gzip), `.tar.zst` (multi-threaded zstd). Both compressors
fan out across `--jobs` cores.

## Configuration (`magebuild.toml`)

Placed at the Magento root, it overrides node fields or adds project steps:

```toml
[build]
jobs = 8

[nodes.di-compile]
fused = true                       # fused interceptors

[nodes.static-deploy]
locales = ["en_US", "nl_NL"]

[nodes.composer-install]
hardlink = true                    # see below — off by default

# add a custom step to the graph
[nodes.my-assets]
after = ["composer-install"]
run = "npm run build"
```

`hardlink` (or the `MAGEBUILD_HARDLINK` env var) makes composer-install
hard-link packages out of a decompress-once store instead of extracting on every
run. It's **off by default** because it only pays off with a *persistent,
uncompressed* store — self-hosted CI with a cache disk, a Docker layer, or
repeated local builds. On ephemeral CI that restores the store from a compressed
`actions/cache`, the restore re-decompresses everything, so a plain extract is
just as fast.

## Deployer integration

Install `magebuild` on the CI build host (curl one-liner above, or the Composer
package), then call it from `deploy.php`. Either run the whole build in one
parallel call:

```php
task('magento:compile', function () {
    run('cd {{release_path}} && magebuild --deployed-version {{content_version}}');
});
```

or swap it in for individual `bin/magento` steps (di-compile ∥ static-deploy
still overlap when run as one call; here they are separate recipe tasks):

```php
task('magento:compile', function () {
    run('cd {{release_path}} && magebuild --only di-compile --skip composer-install');
    run('cd {{release_path}} && composer dump-autoload -o --no-dev');
});
task('magento:deploy:assets', function () {
    run('cd {{release_path}} && magebuild --only static-deploy --skip composer-install'
        . ' --deployed-version {{content_version}}');
});
```

`--skip composer-install` when Deployer already installed the vendor tree; drop
it to let magebuild install from `composer.lock` too.

## How it relates to magequery

[magequery](https://github.com/cresset-tools/magequery) is the read-side tool
(inspect a Magento codebase) and `magecommand` its write-side companion (DI
compile, static deploy). magebuild is the orchestrator: it links those engines
as libraries and runs them, plus Composer, as a parallel build graph.

## License

[EUPL-1.2](LICENSE).
