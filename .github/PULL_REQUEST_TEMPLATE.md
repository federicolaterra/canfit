## Summary

Describe the problem and the smallest useful change.

## Safety

- [ ] No process is closed without explicit confirmation; "never"-class and protected processes are
      never suggested.
- [ ] No telemetry or unrequested network access was added; all data stays local.
- [ ] Every new value shown to the user is labelled measured, read from config, or estimated.
- [ ] No runtime dependency for the user was added (single binary, no admin rights for basic use).

## Verification

- [ ] `make check`
- [ ] Tested on (OS, version, real machine or VM):
- [ ] Documentation and `docs/open-source/CHANGELOG.md` updated
