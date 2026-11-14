# Changelog

## [0.10.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.9.0...magebuild-v0.10.0) (2026-11-14)


### ⚠ BREAKING CHANGES

* **magebuild:** drop Windows support

### Features

* **magebuild:** deploy a release through an agent on the host ([d17e354](https://github.com/cresset-tools/magebuild/commit/d17e354a0a4a256f84b91fd470f298d2b1a9a2fe))
* **magebuild:** drop Windows support ([8095970](https://github.com/cresset-tools/magebuild/commit/80959707c00002f305c1065daa81c7cc12506ce5))

## [0.9.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.8.0...magebuild-v0.9.0) (2026-11-13)


### Features

* **magebuild:** install tar dists and git-source packages ([604b551](https://github.com/cresset-tools/magebuild/commit/604b551bced582c0d5c64319b80a2702af0c5516))


### Bug Fixes

* **magebuild:** apply composer.json patches, not only patches/ ([e8b3a48](https://github.com/cresset-tools/magebuild/commit/e8b3a48e3d202cc4cb698944d9db14bb325d3e53))
* **magecommand:** stop printing the unresolvable-class note on di compile ([5b62a5a](https://github.com/cresset-tools/magebuild/commit/5b62a5a2e2307bfbf214ef1d6b4f5278d43c22f4))

## [0.8.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.7.0...magebuild-v0.8.0) (2026-11-11)


### Features

* **magebuild:** db-status, a native setup:db:status ([429be55](https://github.com/cresset-tools/magebuild/commit/429be556d701c9d88d319fa7ca16ffc9d5e6bc2e))


### Bug Fixes

* **magebuild:** build aarch64-apple-darwin release binaries on macos-15 ([1fa8d5b](https://github.com/cresset-tools/magebuild/commit/1fa8d5b947916e6197c12be19273cea425d33a43))

## [0.7.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.6.0...magebuild-v0.7.0) (2026-11-11)


### Features

* apply the project's patches/ directory during composer-install ([#17](https://github.com/cresset-tools/magebuild/issues/17)) ([4bbbf8c](https://github.com/cresset-tools/magebuild/commit/4bbbf8c9439cb23464d920db9405f2ac322348d6))


### Bug Fixes

* drop the x86_64-apple-darwin target everywhere ([14eaf84](https://github.com/cresset-tools/magebuild/commit/14eaf84d97c9af903201612ce994118bf02f02e2))

## [0.6.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.5.0...magebuild-v0.6.0) (2026-07-24)


### Features

* **static-deploy:** MAGEBUILD_SYMLINK env toggle + document symlink ([1e49e9b](https://github.com/cresset-tools/magebuild/commit/1e49e9b447aab075b2f964563404884d942fd938))
* **static-deploy:** MAGEBUILD_SYMLINK env toggle + document symlink ([a197404](https://github.com/cresset-tools/magebuild/commit/a197404b1a442774a087b063bcb43cbcc0d67a60))

## [0.5.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.4.0...magebuild-v0.5.0) (2026-07-24)


### Features

* adopt magequery 0.15.0 deploy flags + --preset hyva ([58203fc](https://github.com/cresset-tools/magebuild/commit/58203fc3a294d4830f5ea4a17d4082d6d43f19fa))
* **preset:** adopt magequery 0.15.0 deploy flags + --preset hyva ([aff3fff](https://github.com/cresset-tools/magebuild/commit/aff3ffff447954418c5a4be0ef6c5df98ef7a0a3))


### Performance Improvements

* parallelize file reads in the package step ([db27146](https://github.com/cresset-tools/magebuild/commit/db27146bdcab5b43589d6c47a6b4e58d3a496469))
* parallelize file reads in the package step ([c5e67b4](https://github.com/cresset-tools/magebuild/commit/c5e67b43380854e6a7228aed9b678b7d7cf8bf0c))

## [0.4.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.3.1...magebuild-v0.4.0) (2026-07-24)


### Features

* **static-deploy:** add `no_parent` option (magecommand --no-parent) ([d667e36](https://github.com/cresset-tools/magebuild/commit/d667e36b4caced746c3ed7dd0a37d045c8237d47))
* **static-deploy:** add `no_parent` option (magecommand --no-parent) ([fce773c](https://github.com/cresset-tools/magebuild/commit/fce773c6a1fba20131404e6fe2bb7afa28525ed7))

## [0.3.1](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.3.0...magebuild-v0.3.1) (2026-07-22)


### Bug Fixes

* make composer hard-linking opt-in (default Extract) ([cbbee75](https://github.com/cresset-tools/magebuild/commit/cbbee75777cec1a96c09215ad5dcfc82e4bcec99))

## [0.3.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.2.2...magebuild-v0.3.0) (2026-07-22)


### Features

* hard-link composer packages from a warm store in CI ([eec4a86](https://github.com/cresset-tools/magebuild/commit/eec4a86bbb4cb39a12a7b15c083210859126f8cd))


### Bug Fixes

* **deps:** re-pin magequery to 0.13.0 (latest magecommand engines) ([9c4eb9e](https://github.com/cresset-tools/magebuild/commit/9c4eb9e2643e43c137a7fe59fec3e1232530100c))

## [0.2.2](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.2.1...magebuild-v0.2.2) (2026-07-22)


### Bug Fixes

* quieter vendor check + persistent, shareable composer cache ([352bc54](https://github.com/cresset-tools/magebuild/commit/352bc5486b72e5c7b149e7331662b21f28e34d5d))

## [0.2.1](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.2.0...magebuild-v0.2.1) (2026-07-22)


### Bug Fixes

* **deps:** re-pin magequery to include the LESS [@import](https://github.com/import) url() passthrough ([4abbc99](https://github.com/cresset-tools/magebuild/commit/4abbc99cce503794d42eaeaad017cb0d01ee8b12))

## [0.2.0](https://github.com/cresset-tools/magebuild/compare/magebuild-v0.1.0...magebuild-v0.2.0) (2026-07-22)


### Features

* magebuild, a parallel DAG-ordered Magento build orchestrator ([14129ce](https://github.com/cresset-tools/magebuild/commit/14129ceef5013768f37540bfeca38dafa59c5e01))
* **package:** deployed-version, parallel gzip, and zstd compression ([0083d6a](https://github.com/cresset-tools/magebuild/commit/0083d6a32840c16cebb9eacc80759efc5fa0a54b))
