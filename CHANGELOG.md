# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-05-09

### Added

- Added newline-delimited JSON status updates over the recording Unix socket.
- Added socket status states for `recording`, `processing`, `complete`, and
  `error`.

### Changed

- Changed `mnemo stop` to use the JSON socket protocol instead of the previous
  plain-text `stop` command.

## [0.2.0] - 2026-05-09

### Added

- Added profile-based macOS Keychain storage for ElevenLabs and Hindsight API
  keys.
- Added `mnemo keychain sync`, `mnemo keychain list`, and
  `mnemo keychain remove` commands.
- Added support for resolving Hindsight API keys from Keychain when retaining
  transcripts.

### Changed

- Changed Keychain account names to include profile and key identity, such as
  `profile:default:elevenlabs-api-key` and
  `profile:default:hindsight-api-key`.
- Documented API key precedence and Keychain setup for GUI/plugin-launched
  workflows.

## [0.1.0] - 2026-05-09

### Added

- Initial macOS Apple Silicon release.
