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
curl -LsSf https://bougie.tools/magebuild.sh | sh
```

Prebuilt binaries ship for Linux (gnu and musl, x86_64 and arm64), macOS
(Apple Silicon). Windows is not supported. Every release archive is
Sigstore-signed.

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
magebuild --report build.json          # also write states, timings and diagnostics, for CI
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
target_php = "8.3"                 # the PHP the store runs; else composer.json's config.platform.php
target_platform = "mageos-4.0"     # magento-2.4.9, mageos-3.0, mageos-3.3, mageos-4.0; else read off setup/
area_config = "full"               # "full" or "diff" (4.0+ only); else the platform's own
fail_on = ["lost-arguments"]       # for a CI gate; see "Failing the build" below

[nodes.static-deploy]
locales = ["en_US", "nl_NL"]
no_parent = true                   # don't deploy a theme's parent(s); see below
symlink = "dir"                    # link instead of copying: "file" (= true) or "dir"; see below
fail_on = ["not-deployed", "skipped-theme"]

[doctor]                           # run magequery doctor before di-compile generates; present = on
deny = ["E1004"]                   # these lints fail the build too (code or name)
fail_on_warnings = false           # warnings fail the build too
baseline = "magequery-baseline.json"

[nodes.composer-install]
hardlink = true                    # see below — off by default

# add a custom step to the graph
[nodes.my-assets]
after = ["composer-install"]
run = "npm run build"
```

The di-compile `target_*` and `area_config` settings are `magecommand di compile`'s
`--target-php`, `--target-platform` and `--area-config`: for a build whose checkout
doesn't say what the store runs. A platform name covers a range of releases, not one
version. `area_config = "diff"` writes every area but global as its differences from
global, which only a Mage-OS 4.0+ runtime can read; it's the 4.0 default, and
di-compile refuses it for any other platform.

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

`symlink` (static-deploy, off by default) stops the deploy writing the same
bytes once per theme and locale. It has two modes:

- `"file"` (or `true`; implied by `--preset hyva`) maps to magecommand's
  `--symlink=file`: pure-copy files (images, fonts, verbatim JS) become
  **relative symlinks** back to their `vendor/`/`app/`/`lib/web/` source; only
  derived files (LESS-compiled CSS, generated RequireJS, bundles) stay real.
  The tarball ships `vendor` beside `pub/static`, so the links resolve after
  extraction to any path and survive an atomic `current → releases/N` swap.
- `"dir"` maps to `--symlink=dir`: every package directory (`Magento_Catalog/`,
  `js/`, …) that is byte-identical to one already deployed becomes **one
  relative symlink** to it, at the highest level where it matches. Files are
  real copies, written once, and everything stays inside `pub/static`.

Both serve the same bytes at the same URLs. `"file"` is still one link per file
per theme per locale, and on a store with many themes creating those links is
most of the deploy: a 30-theme, 2-locale shop wrote 576,000 symlinks and
144,000 directories with `"file"`, and 24,700 links, 14,400 files and 4,300
directories with `"dir"`. On six cores its build went from 19.4 s to 11.8 s
(static deploy 15.5 s to 10.4 s, package 3.9 s to 1.4 s) and the artifact from
740 MB and 785,000 entries to 452 MB and 244,000. `"dir"` is the one to pick unless something edits deployed files in
place (an edit shows in every package that links to that directory).

The `MAGEBUILD_SYMLINK` env var turns linking on where the config leaves it off:
`MAGEBUILD_SYMLINK=dir` for directory links, any other value for file links.
Either way the serve-time web server must follow symlinks (nginx's default,
`disable_symlinks off`).

### Failing the build

As a CI gate, a build should fail whenever the store would not work. Three things
decide that:

- **What `setup:di:compile` refuses, di-compile refuses too**, always, writing
  nothing: a preference whose type isn't a class (or isn't compiled), and a plugin
  class that doesn't exist. The error is Magento's, with the di.xml file and line.
- **`fail_on`** fails a node on a result Magento's own command accepts but that
  breaks the store at runtime. di-compile: `lost-arguments`, types whose di.xml
  arguments the compiled config leaves out (Magento builds them by reflection
  without them). static-deploy: `not-deployed`, a stylesheet that did not compile
  and is missing from `pub/static`; `skipped-theme`, a theme left out because its
  parent chain is broken. Unset, these are warnings, as with `bin/magento`.
- **`[doctor]`** runs `magequery doctor` inside di-compile, on the store it has
  already loaded, before anything is generated. Doctor's errors fail the build
  (a used virtual type or an object argument naming a missing class, among the
  rest); `deny`, `fail_on_warnings`, `baseline`, `source` and `in` are `magequery
  doctor`'s flags of those names. It needs the built-in di-compile node.

`--report <file.json>` writes the run as data for CI, failed or not: each node's
state, duration and the diagnostics its step found, each with a `kind`
(`rejected`, `lost-arguments`, `compile-finding`, `doctor`, `doctor-baseline`;
`not-deployed`, `skipped-theme`, `less-warning`), a `severity` (`error` for what
failed the build), the `message`, and where known a `code` (doctor's `E2006`, a
rejection's `preference-missing`), `subject`, root-relative `file` and `line`,
`areas`, `theme` and `locale`. A build that fails before any node starts writes
one too, with the reason as the top-level `error` and no nodes.

```json
{ "version": 1, "failed": true, "wall_ms": 812,
  "nodes": [ { "id": "di-compile", "state": "failed", "duration_ms": 640,
               "error": "di-compile: setup:di:compile refuses this configuration too (1 error(s)); nothing was written",
               "diagnostics": [ { "kind": "rejected", "severity": "error", "code": "preference-missing",
                                  "file": "app/code/Acme/Probe/etc/di.xml", "line": 3,
                                  "message": "Preference declared for …, but the latter does not exist." } ] } ] }
```

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

### Gating `setup:upgrade` with `db status`

The status checks below are [magecommand](https://github.com/cresset-tools/magequery)
commands: magebuild runs them itself during `magebuild deploy`, and magecommand
is the tool for running one on its own.

`magecommand db status` is a native `bin/magento setup:db:status`: it compares the
declarative schema, the setup patches and the module versions against the
database named in `app/etc/env.php`, in tens of milliseconds rather than a
framework bootstrap. It uses the same exit codes as Magento: 0 when the
database is up to date, 2 when an upgrade is required (including when the
database is unreachable), 1 on an error such as a store that isn't installed.
`--json` prints what differs.

It runs on the deploy target, so put `magecommand` on that host's `PATH`
(`curl -LsSf https://bougie.tools/magequery.sh | sh`), then override the
recipe's check in `deploy.php`:

```php
use Deployer\Exception\RunException;

set('database_upgrade_needed', function () {
    try {
        run('magecommand db status --root {{release_or_current_path}}');
    } catch (RunException $e) {
        if ($e->getExitCode() == 2) { // upgrade required
            return true;
        }
        throw $e;
    }
    return false;
});
```


### Gating `app:config:import` with `config status`

`magecommand config status` is a native `bin/magento app:config:status`. Like
Magento, it hashes each imported section of the deployment config (`scopes`,
`themes`, `system`, plus any section a module registers with `ImporterPool`)
and compares it with the hash the last `app:config:import` stored in the `flag`
table. Exit codes match Magento's: 0 when up to date, 2 when an import is
needed. It exits 1 on an error, and also when the config holds something it
doesn't reproduce exactly (a PHP constant, an exponent-form float,
`MAGENTO_DC__OVERRIDE`); run `bin/magento app:config:status` then. In
`deploy.php`, the recipe's `config_import_needed` takes it the same way as
`db status` above.

### Themes and queues: `theme status` and `queue status`

`setup:db:status` doesn't look at themes or message queues, so a release that
only adds one of them looks up to date to it. Mage-OS added
`theme:registration:status` and `queue:config:status` for this, and these are
native versions with the same exit codes (0 when nothing is missing, 2 when
`setup:upgrade` has something to register):

- `magecommand theme status` lists the themes that a `registration.php` declares but the
  `theme` table doesn't hold;
- `magecommand queue status` lists the topology queues (`queue_topology.xml`, plus the
  older `queue.xml` when `Magento_Amqp` is enabled) missing from the `queue`
  table.

RabbitMQ keeps its own state, which these commands don't read. Instead,
`magebuild deploy` fingerprints what Magento declares there from (env.php's
`queue` section and the modules' `queue_topology.xml`, `queue.xml` and
`communication.xml`), keeps the live release's fingerprint in
`.dep/magebuild-amqp-topology`, and upgrades when it changes. On a store with
RabbitMQ, the first deploy with this upgrades once. A queue deleted on the
broker by hand goes unnoticed. `magecommand queue status` prints the fingerprint.
## Deploying (`magebuild deploy`)

`magebuild deploy` replaces Deployer's deploy stage. It opens one ssh session
to the host and runs a small agent there: the same magebuild binary, uploaded
to `~/.cache/magebuild/agent-<sha256>` only when that build is not there yet.
The agent does the release work that a Deployer recipe does one remote command
at a time, so a deploy costs one round trip instead of about a hundred:

1. takes a lock (`flock` on `.dep/magebuild.lock`, released however the agent
   exits) and creates `releases/<N>`;
2. reuses every file the live release already has and unpacks only the rest
   as it streams in (see "Sending only what changed" below);
3. links `shared/` into the release, derives `app/etc/env.php` from a template
   with a fresh cache `id_prefix` per release, and sets writable dirs;
4. checks natively whether the database needs an upgrade (`db status`), the
   config an import (`config status`), and whether there is a theme to register
   or a queue to set up (`theme status`, `queue status`, and the RabbitMQ
   topology fingerprint below), asking `bin/magento app:config:status` only when
   the config holds something the native check can't hash exactly;
5. pauses cron, runs the import or upgrade only when needed (maintenance mode on
   the live release meanwhile, or with `maintenance = "when-needed"` only for
   the part that needs it; see "Upgrading with the store open"), then swaps `current` atomically. Maintenance
   stays on through the swap, so the old code never serves the upgraded
   database: the flag is set in both the live and the new release, and after
   the swap and the opcache reset only the new release's is cleared. The old
   release keeps its flag, so rolling back to it after an upgrade comes up in
   maintenance until you run `maintenance:disable` there. If the live release
   was already in maintenance before the deploy, the new one inherits it and
   the deploy leaves it on. If an import or upgrade fails partway, the database
   may match neither release, so the agent fails closed: maintenance stays on,
   cron stays stopped, and `.dep/upgrade-failed` tells the next deploy to
   clear that flag once it gets through. Once an upgrade has started, or the
   swap is next, the agent finishes even if the ssh connection drops (CI
   cancelled or timed out): it ignores SIGHUP, keeps PHP's output off the
   dead connection, and writes how the deploy ended to `.dep/last-deploy.log`.
   Before that point a dropped connection still stops the deploy with the
   store untouched. The agent sets and clears `var/.maintenance.flag` itself, exactly as
   `maintenance:enable`/`disable` do, without booting PHP. Like Deployer's
   recipe, share `var/.maintenance.ip` but not the flag: a missing shared file
   is created empty, which is maintenance on, and Magento's disable deletes the
   link rather than the shared file;
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
maintenance = "when-needed"                     # default "always"; see below

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

### Upgrading with the store open

With `maintenance = "when-needed"`, the check also judges each declarative
schema change (see `magecommand db status`): whether code written for the old
schema keeps working on the new one, and whether the server applies it without
blocking writes. Added nullable columns, added indexes, new tables and longer
varchars pass; dropped or renamed columns and tables, NOT NULL columns without
a default, narrower types and unique or foreign keys don't. When every schema
change passes, the agent applies just the schema changes before maintenance,
with a small PHP script that computes Magento's own diff and runs it only if it
is exactly the list that was checked (otherwise nothing changes and the
upgrade runs in maintenance as usual). Maintenance then covers only what is
left: schema and data patches, old-style setup scripts, a config import, or a
schema change that did not pass. When nothing is left, the whole upgrade runs
with the store open. Recurring setup scripts are not judged; they run with the
rest of the upgrade.

If applying the schema changes with the store open fails, the deploy stops
without maintenance: what was applied only adds to the schema, so the live
code keeps working, and the next deploy applies the rest. The default,
`maintenance = "always"`, keeps the store in maintenance for the whole upgrade.

### Sending only what changed

The deploy works from a manifest of the artifact: every path with its mode
and the sha256 of its contents. The package step writes it beside the
artifact as it packs (`artifact.tar.zst` → `artifact.tar.zst.manifest`), so
keep the two together when the deploy runs elsewhere. The runner uses that
file only when it was written for these exact bytes, and otherwise reads the
manifest out of the artifact, which takes about a second longer. The agent compares it with
the manifest it stored for the live release (`.dep/manifests/<N>.json.zst`)
and hard-links each file whose content and mode are unchanged from the live
release, so the runner sends a zstd tar of only the new and changed files.
A rebuild with no input changes sends a few KB instead of the whole artifact.
The first deploy, or one whose live release has no stored manifest, sends the
whole artifact as before.

Linking costs one `link()` per unchanged file, about a second for a Magento
release on a small VM, so it happens ahead of time. After each deploy the
detached cleanup builds a copy of the new live release in
`releases/.next-<N>` (hard links to its files, its symlinks, a copy of
`app/etc/`; not the shared links or the derived env.php), under a `.tmp` name
until it is complete. The next deploy renames that copy into place and removes
or replaces only what changed, checking each file it keeps against the size and
mtime recorded when the copy was made. Without a complete copy, it links from
the live release as above. The copy is one more tree of directories and links
on disk; with `reuse = "copy"` it is a full copy.

Hard-linked files are the same file in every release that has them, so nothing
may change a release's files in place after its deploy. `app/etc/` is always
copied rather than linked, because `bin/magento` may rewrite `config.php`. A
file edited on the host since its deploy no longer matches the size and mtime
in its stored manifest, so it is sent again rather than linked. On hosts where
something does write into releases, `reuse = "copy"` gives every release its
own copy of each file; that costs disk, but nothing extra on the network.

`shared_dirs` and `writable_dirs` default to Deployer's magento2 recipe;
`shared_files` defaults to just `var/.maintenance.ip`. `[deploy.env_php]` is
required, and `app/etc/env.php` may not be shared or linked: each release's
env.php is derived so it gets its own cache prefix, and the new release's
commands never touch the live release's cache. A host that shares env.php under
Deployer already has it at `shared/app/etc/env.php`, so that path is the template.
`upgrade = "refuse"` refuses to swap when an upgrade or import is needed instead
of running it, leaving the live release untouched (`"check"`, the old name, still
works). `--agent <path>` picks the binary to run on the host (a static
Linux build; by default the running magebuild), `--host` overrides the host,
and `--ssh` the ssh command.

## How it relates to magequery

[magequery](https://github.com/cresset-tools/magequery) is the read-side tool
(inspect a Magento codebase) and `magecommand` its write-side companion (DI
compile, static deploy). magebuild is the orchestrator: it links those engines
as libraries and runs them, plus Composer, as a parallel build graph.

## License

[EUPL-1.2](LICENSE).
