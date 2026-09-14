# Changelog

Bench is an application, not a library: versions mark what a build can do,
and nothing here is a compatibility promise to other code. The crates in this
workspace are not published.

## [0.1.1] - 2026-09-14

### Fixed

- **The trigger rolled on noisy signals as if it were off.** It chose the
  newest complete edge every frame, and on a signal with dense crossings (a
  noise floor at full scale, a tone buried in noise) there is always a fresh
  edge just before that boundary, so the display tracked live and the toggle
  had no visible effect. A sweep now runs to its end before the trigger
  re-arms, and the next sweep starts at the first edge after it: a periodic
  signal stands still, a noisy one steps sweep by sweep, and a held edge that
  has left the ring re-acquires near live. Edge selection moved to
  `bench_core::trigger` with tests that reproduce the roll.
- Smart Panel tool refs use the gateway's canonical names,
  `smart-panels.list` and `smart-panels.publish`.

## [0.1.0] - 2026-09-14

Initial public release. A DSP workbench that runs on DataGrout, and the
reference example of an application built on the gateway.

### The instrument

- **Sources.** A function generator with stackable components (sine, square,
  triangle, noise), presets, a noise floor and a slow-motion clock; live audio
  input behind `--features audio`, with device, channel, gain and a level
  meter; WAV or CSV replay by dropping a file on the window.
- **Scope.** Time base and analysis frame; a rising-edge trigger that holds
  its last alignment when no edge is found; a draggable gate whose edges
  resize the capture; zoom about the pointer, pan, minimap, crosshair readout;
  Space, Enter and Home on the keyboard.
- **Meter.** RMS, peak-to-peak, DC offset, and frequency, period and duty
  cycle from the analysis frame, every frame, locally, with the chain's
  dominant frequency and THD beside them. A missing reading shows as a dash,
  never as zero.
- **Capture.** The analysis frame to `~/Documents/Bench/captures` as WAV and
  CSV.
- **Profiles.** The whole setup as one `.bench` file: source and knobs, chain
  and arguments, time base, frame, trigger, placed panels. A Profiles menu
  with the shipped examples and the user's saved ones, save-current-as, drop
  to load, `--profile` to launch. Never persists auto-analysis on. The four
  examples in `bench-core/examples/profiles/` are compiled into the binary.

### The chain

- **Steps.** `signal.filter` (FIR, run causal), `signal.iir`,
  `signal.hilbert`, `signal.resample`, `signal.spectral`, `signal.convolve`,
  `signal.correlate` (by lag, or `acf`/`pacf`), `signal.embed`,
  `signal.peaks`, `linalg.pca`, `linalg.cluster`, `linalg.normalize`,
  `math.window`, `math.normalize`, `math.describe`.
- **Shape typing.** A step can only be appended where its input shape matches;
  an invalid chain cannot be built.
- **One plan, two authors.** The chain compiles to the `flow.into` plan an
  agent would submit, shown as "the agent's call"; frames travel as packed
  floats where the tool decodes them.
- **Results.** Every stage charted; a step the tool refused says why in its own
  slot; a filter's delay or look-ahead is stated in samples; oversized results
  are fetched from the gateway's cache rather than dropped.
- **Wire weight.** Every series step asks the gateway for `records: false`:
  Bench charts from `values`, and the `records` projection of the same numbers
  was about two-thirds of a 340 KB run over a 1,024-sample frame. The spectrum
  keeps its records, which are the bins. The run itself asks for
  `step_outputs: false`, because the full envelope carries every step result
  twice and Bench reads the keyed copy. A gateway that predates either switch
  ignores it.
- **Handles.** Filter cutoffs are drawn on the spectrum and set by dragging.
- **Facts and skills.** Derived features go to a logic cell as facts; a spec
  is derived from them; a chain mints as a skill with a certificate, served
  locally by `bench-serve` at `/skills/<slug>`.

### The gateway

- Browser sign-in with OAuth authorization code + PKCE through
  `datagrout-conduit` 0.8.1; rotated refresh tokens are written back; an
  expired grant is found at launch, not after a chain is built; a dropped MCP
  session re-handshakes once.
- Smart Panels from the account placed into a resizable dock, through
  `datagrout-panels` 0.2.1. Loading is explicit and priced: the list is one
  call, rows are one query per data panel.

### Platforms

- macOS, Linux and Windows, each in CI with and without the `audio` feature.
  Windows needs no package step (WASAPI, rustls). Paths come from one place:
  `HOME` then `USERPROFILE` for Documents, `$XDG_CONFIG_HOME`, `%APPDATA%` or
  `~/.config` for the sign-in file. Reading `HOME` alone had put every
  capture and profile in `%TEMP%` on Windows.

### Gateway facts

Found live rather than in a schema, and each one cost a turn:

- `flow.into` resolves `$step_N.field`, not `$sN`; binding the whole step
  result hands the next tool an object it refuses, while the flow still
  reports `completed`.
- A flow that fails mid-way returns `execution_error` and no
  `execution_result`; the steps that succeeded are only inside the error.
- `signal.*` and `linalg.*` decode `X_b64` packed floats, and so does
  `math.window` through its `data` input; `math.normalize` does not, and
  `math.describe` reads `payload`.
- `signal.filter` is zero-phase by default and uses `(taps−1)/2` samples of
  look-ahead: right for a file, wrong for a live trace. `causal: true` delays
  instead; either way the result says which in `delay_samples` and
  `look_ahead_samples`.
- Results over ~48 KB come back as a `cache_ref`; `prism.paginate` returns
  the full envelope as one record for one credit.
- The gateway rotates refresh tokens; a process that refreshes and does not
  persist the new grant leaves a consumed token on disk.
