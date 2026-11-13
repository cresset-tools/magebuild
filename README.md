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
(Apple Silicon), and Windows. Every release archive is Sigstore-signed.

### Composer (for Deployer / CI projects)

```sh
composer require --dev cresset/magebuild
```

This puts `magebuild` in `vendor/bin/`; the package fetches the prebuilt binary
for your platform on first run (checksum-verified, cached).

### Without installing (bgx)

[bougie](https://bougie.tools)'s `bgx` (like npx) runs magebuild in an isolated,
globally-cached environment without adding it to your project. Everything after
the package is forwarded straight to magebuild:

```console
$ bgx cresset/magebuild plan
$ bgx cresset/magebuild --artifact release.tar.zst
```

## Usage

```sh
magebuild                              # build the Magento root in the current dir
magebuild --root path/to/store         # ... or an explicit root
magebuild --artifact release.tar.zst   # also package (gzip/zstd chosen by extension)
magebuild plan                         # print the DAG + parallel schedule, run nothing
magebuild node di-compile              # run one node and its dependencies
magebuild --json                       # machine-readable output
magebuild db-status                    # does the database need setup:upgrade?
magebuild deploy --from release.tar.gz # ship a built release to the [deploy] host
```

Common flags: `--jobs N`, `--only <ids>`, `--skip <ids>`, `--dry-run`,
`--profile`, `--preset <name>`, `--deployed-version <v>`, `--exclude-from <file>`.

### Presets

`--preset hyva` shapes the graph for a [Hyvä](https://hyva.io) production build,
following the Hyvä deploy docs:

1. a Tailwind `npm ci --ignore-scripts && npm run build` per discovered Hyvä
   theme (`app/design/frontend/*/*/web/tailwind` if you have a project theme,
   else `vendor/hyva-themes/*/web/tailwind`), wired to run before static-deploy;
2. a static-content deploy with `--no-parent --no-less --no-js-bundle
   --no-html-minify --symlink=file` — Hyvä ships pre-built Tailwind CSS and no
   RequireJS bundles, and `--symlink=file` makes every byte-identical asset a
   relative symlink to its `vendor/app/lib` source instead of copying it
   (smaller, faster; the artifact already ships `vendor` beside `pub/static`).

The preset is applied over the built-in defaults but **under** `magebuild.toml`,
so any field it sets can still be overridden per node.

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
no_parent = true                   # don't deploy a theme's parent(s); see below
symlink = true                     # symlink pure copies to source; see below

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

**Composer patches.** composer-install applies a project's `patches/` directory
automatically (zero-config — the same convention as `bigbridge/patcher`), so the
installed tree matches a real `composer install` even though the in-process
installer skips composer plugins. Each `*.patch` file's target package and strip
depth are inferred from its diff; package patches apply inside `vendor/<pkg>`,
project-root patches at the root. Application is idempotent — the applied set is
fingerprinted in `patches.lock.json`, so a re-run only re-patches packages whose
patch set changed, and a failing patch aborts the build (never a silently
unpatched tree). Projects with no `patches/` dir are unaffected. This is what
makes, e.g., a Hyvä store's checkout modules Tailwind-v4-clean before the
`--preset hyva` Tailwind build runs.

`no_parent` (static-deploy, off by default) maps to magecommand's `--no-parent`:
a child theme normally pulls its ancestor themes into the deploy (Magento's quick
strategy). With it **on**, only the themes you deploy are emitted. Static deploy
resolves the parent→child fallback from *source* at deploy time, so the child
tree is self-contained and the parent theme's own `pub/static` output is
redundant when nothing serves it — e.g. a Hyvä storefront that only touches
`Magento/luma` via the fallback checkout doesn't need `Magento/blank` shipped.
Leave it off unless you know the parent is never requested at runtime.

`symlink` (static-deploy, off by default; also the `MAGEBUILD_SYMLINK` env var,
and implied by `--preset hyva`) maps to magecommand's `--symlink=file`: pure-copy
files — images, fonts, verbatim JS — become **relative symlinks** back to their
`vendor/`/`app/`/`lib/web/` source instead of duplicated bytes; only derived
files (LESS-compiled CSS, generated RequireJS, bundles) stay real. It's well
suited to a magebuild artifact — the tarball ships `vendor` beside `pub/static`,
so the relative links resolve after extraction to any path and survive an atomic
`current → releases/N` swap. On a large multi-store the win is significant: the
deploy stops writing, and `package` stops re-reading, the gigabytes of duplicated
asset bytes, so the artifact shrinks several-fold (≈3.5× on one production
multi-store tree: 2.6 GiB → 0.7 GiB). The one requirement is that the serve-time
web server follows symlinks (nginx's default, `disable_symlinks off`); leave it
off for a bare `pub/static` shipped without its source tree.

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

### Gating `setup:upgrade` with `db-status`

`magebuild db-status` is a native `bin/magento setup:db:status`: it compares the
declarative schema, the setup patches and the module versions against the
database named in `app/etc/env.php`, in tens of milliseconds rather than a
framework bootstrap. It uses the same exit codes as Magento: 0 when the
database is up to date, 2 when an upgrade is required (including when the
database is unreachable), 1 on an error such as a store that isn't installed.
`--json` prints what differs.

It runs on the deploy target, so put `magebuild` on that host's `PATH` (the
curl installer above), then override the recipe's check in `deploy.php`:

```php
use Deployer\Exception\RunException;

set('database_upgrade_needed', function () {
    try {
        run('magebuild db-status --root {{release_or_current_path}}');
    } catch (RunException $e) {
        if ($e->getExitCode() == 2) { // upgrade required
            return true;
        }
        throw $e;
    }
    return false;
});
```

## Deploying (`magebuild deploy`)

`magebuild deploy` replaces Deployer's deploy stage. It opens one ssh session
to the host and runs a small agent there: the same magebuild binary, uploaded
to `~/.cache/magebuild/agent-<sha256>` only when that build is not there yet.
The agent does the release work that a Deployer recipe does one remote command
at a time, so a deploy costs one round trip instead of about a hundred:

1. takes a lock (`flock` on `.dep/magebuild.lock`, released however the agent
   exits) and creates `releases/<N>`;
2. unpacks the artifact as it streams in;
3. links `shared/` into the release, derives `app/etc/env.php` from a template
   with a fresh cache `id_prefix` per release, and sets writable dirs;
4. checks for a database upgrade natively (`db-status`) and for a config import
   (`bin/magento app:config:status`);
5. pauses cron, runs the import or upgrade only when needed (maintenance mode on
   the live release meanwhile), then swaps `current` atomically;
6. resets opcache over FastCGI, deletes the previous release's cache keys,
   resumes cron, and sets old releases aside for the next deploy to delete
   while it uploads.

The layout is Deployer's (`releases/<N>`, `shared/`, a relative `current`,
`.dep/latest_release`, `.dep/releases_log`), so a Deployer recipe still works
on the same host as a fallback.

```toml
[deploy]
host = "magento@shop.example.com"
path = "/var/www/shop"
php = "/usr/bin/php8.4"
shared_files = ["var/.maintenance.ip"]          # env.php is derived, below
links = { "auth.json" = "shared/auth.json" }    # linked, never touched
keep_releases = 5

[deploy.env_php]
source = "shared/app/etc/env.php"               # rolls cache/frontend/*/id_prefix to r<N>_

[deploy.cron]                                   # default: backend = "none"
backend = "systemd"
stop = ["magento-cron.timer", "magento-cron.service"]
start = ["magento-cron.timer"]

[deploy.opcache]
fastcgi = "127.0.0.1:9000"                      # or a socket path
tmp_dir = "/var/www/tmp"                        # a dir php-fpm sees at the same path
```

`shared_dirs` and `writable_dirs` default to Deployer's magento2 recipe.
`upgrade = "check"` refuses to swap when an upgrade or import is needed instead
of running it. `--agent <path>` picks the binary to run on the host (a static
Linux build; by default the running magebuild), `--host` overrides the host,
and `--ssh` the ssh command.

## How it relates to magequery

[magequery](https://github.com/cresset-tools/magequery) is the read-side tool
(inspect a Magento codebase) and `magecommand` its write-side companion (DI
compile, static deploy). magebuild is the orchestrator: it links those engines
as libraries and runs them, plus Composer, as a parallel build graph.

## License

[EUPL-1.2](LICENSE).
