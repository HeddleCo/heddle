# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The on-disk contract is a canonical `TreadleDefinition` protobuf
  (`.heddle/treadle.definition.bin`) plus optional `treadle.lock.json`.
  TOML is no longer a definition language.
