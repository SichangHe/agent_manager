# OmniGent native Codex ownership

(authored by agents unless marked 🧑)

- 🧑 "Should use Omnigent by default, tmux as an option"
- latest Codex creates ephemeral auxiliary threads to generate task titles
  - these are not user `/clear` requests and must not change the OmniGent conversation
  - `params.thread.ephemeral=true` is the protocol discriminator, not prompt text or model name
- OmniGent 0.11.0 local compatibility patch
  - `~/.config/omnigent_patches/omnigent-codex-ephemeral-threads.patch` filters ephemeral notifications during initial discovery and later ownership rotation
  - native subagent threads are also excluded from initial discovery
  - normal persistent threads and `/clear` still use existing ownership transfer
  - applied to the local OmniGent package used by the host's Python interpreter; new runners import the patched module without restarting existing workers
  - recheck this compatibility patch after reinstalling or upgrading OmniGent
  - upstream PR #5648 already fixes ephemeral ownership rotation and is included in v0.16.0; the older local 0.11.0 installation lacked it
  - upstream initial thread discovery still lacks the auxiliary-thread filter; the local extra startup guard is separate from the reproduced rotation bug
  - apply only to the unpatched OmniGent 0.11.0 source: first check `patch --dry-run --forward --fuzz=0 -p1 -i PATCH`, then use the same command without `--dry-run`
  - stop on any failed check; never retry with fuzz or apply twice
- runtime regression
  - fresh latest Codex worker, first prompt, assistant reply on the original OmniGent session, unchanged native thread and terminal ownership
  - `tests/test_omnigent_codex_ephemeral.py` exercises actual installed forwarder notification handling
- inherited command search path preserves first-entry precedence while removing duplicate directories
  - repeated agent launches otherwise exceed OmniGent's per-argument input bound
- native remote terminal does not accept `--no-daemon`
  - Codex rejects it with `--remote`; standalone tmux commands keep the flag
