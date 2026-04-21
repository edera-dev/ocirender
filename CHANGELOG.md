# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/edera-dev/ocirender/compare/v0.2.0...v0.2.1) - 2026-04-21

### Fixed

- *(overlay)* handle case where symlink replaces a directory

### Other
- *(cargo)* update sha2 dependency
- *(cargo)* dependency updates

## [0.2.0](https://github.com/edera-dev/ocirender/compare/v0.1.2...v0.2.0) - 2026-03-18

### Added

- *(cli)* implement registry pull/fetch support

### Fixed

- *(cli)* add doc=false for the cargo-cli bin output
- *(cli)* use better 'about' string
- *(Cargo.toml)* always optimize dependencies
- *(canonical)* don't assume PAX values are UTF-8

## [0.1.2](https://github.com/edera-dev/ocirender/compare/v0.1.1...v0.1.2) - 2026-03-14

### Fixed

- *(overlay)* handle top-level `.` directory entry
- *(squashfs)* capture stderr from squashfs in error output
- *(image)* handle docker-centric media types for manifests

### Other

- Merge pull request #4 from edera-dev/dependabot/github_actions/step-security/harden-runner-2.15.1
- *(deps)* bump step-security/harden-runner from 2.14.2 to 2.15.1

## [0.1.1](https://github.com/edera-dev/ocirender/compare/v0.1.0...v0.1.1) - 2026-03-13

### Other

- re-enable aarch64 testing
