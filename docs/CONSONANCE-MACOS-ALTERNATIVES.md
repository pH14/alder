# Consonance on macOS / Apple Silicon (M3+): Alternatives Design Memo

Status: **v3**, for review. v1 2026-08-06 (task brief + Apple-doc
verification); v2 same day after cross-review against an independent memo
with repo + SDK access; v3 same day after a second round of that
cross-review, which corrected two real defects in the B design (dynamic-PC
uniqueness under uninstrumented assembly; the x28 register choice), updated
the kpc history (the EL-filtered leg *was* run — guest-blind), and split B
into a correctness baseline (B0) and an optimized fast path (B1). §0.1 has
the change log; §9 has the full accept/reject adjudication for both rounds.

Evidence labels: [VERIFIED-DOC] checked against Apple's online docs
2026-08-06 (§8); [SDK] cited from the local macOS 26.4.1 SDK headers by the
cross-review, re-verify in CI; [REPO-EVID] measured results retained in the
Consonance repo (spike branch commits `500f95d`, `8501faa1`; bead `hm-ssz`);
[XNU] apple-oss-distributions/xnu at `f6217f89…`, re-verify against
shipping; [LINUX] torvalds/linux mainline source; [ARM] Arm architecture
documentation; [CITED] third-party shipping source; [MEASURE M-nn]
on-hardware only, ledger §7. Where a design names a counter, event, or
mechanism, that exact one is meant; no silent substitution.

---

## 0. Verdict

No credible hardware work-clock path exists on shipping macOS for
M3-or-later — now supported by measurement on both counter banks, not just
absence of API. Two software paths preserve the bit-identical contract, and
three independent review rounds have converged on them:

1. **Primary — Alternative B: a guest-carried software work clock on
   direct-EL1 Hypervisor.framework**, now split into:
   - **B0, the correctness baseline:** a versioned clock (`SW_EDGE_v1`)
     checked at *every* validated chunk with an NZCV-neutral three-
     instruction sequence ending in a reserved `BRK`; the authoritative
     stop is the `BRK` itself, before the chunk's first original
     instruction. No single-step dependency, no sparse-gap reasoning, no
     flag-liveness proof. Budget state in always-resident per-vCPU memory
     (or an audited register — explicitly *not* `x28`; see §3B).
   - **B1, the optimized sparse-guard path (only after B0 passes):** an
     absolute site counter plus a guard graph with a mechanically proven
     maximum gap, armed so the first `BRK` provably lands at `work < W`,
     then qualified single-step to `work == W`. If Apple's step behavior
     fails qualification, B1 dies and B0 survives.
   Public API only; `com.apple.security.hypervisor`; no root, no SIP
   change. Performance: **"credible low-overhead research path"** — no
   ratio is claimed until the full instrumented Linux image is measured
   (M-01); the planning envelope stays 1.2–5× with demote-don't-relax.
2. **Parallel fallback — E1: deterministic x86-64 TCG** with an exact
   translated `BR_INST_RETIRED.CONDITIONAL` clock; ~10–100×; no Apple
   virtualization surface at all.

Closed or demoted: **A** (kpc/kperf) is a measured NO-GO on both banks —
configurable counters guest-blind across all EL masks, fixed counters
unfilterably contaminated [REPO-EVID] — leaving only a half-day corrected
RAWPMU probe, now with the added gate that RAWPMU must demonstrate exact
guest-only counting *before* any PMI/period test is worth running. **C**
(debug stepping) is B1's landing primitive and a slow oracle behind its
hardware gate. **D** (vEL2 sans PMU) closed by construction. **F**
(replay-only) folds into B/E with a sharpened contract: portable films
require software-authoritative V-time on *both* platforms (§3F). **G**
(Asahi bare metal) adjacent, not macOS, M3/M4 PMU still TBA.

### 0.1 Change log

v1 → v2 (first cross-review round): kpc withdrawn as a spike (guest-blind
configurable bank, unfilterable fixed bank [REPO-EVID][XNU]); B's deadline
carrier moved off the debug channel onto HVC/SVC; LL/SC excluded (exclusive-
monitor clearing); pending-IRQ per-run clearing ⇒ software latch [SDK];
WFI/WFE compiled out; fail-closed negative tests; E split into x86-64
product fallback (film-compat) + arm64 oracle; clock-carrier A/B opened.

v2 → v3 (second round):

1. **B split into B0/B1.** v2's single design mixed two sound formulations
   (quantized injection at guard sites; exact landing via arm-early + step)
   in a way that invited misreading and hid the proof obligations. B0 makes
   the per-chunk check universal and the `BRK` authoritative; B1 carries
   the sparse-guard optimization with its guard-graph and `work < W`
   invariants stated as host-checked assertions.
2. **Dynamic-position uniqueness corrected.** v2 claimed (W, PC) names a
   unique dynamic instruction while permitting uninstrumented assembly.
   False as stated: an uninstrumented loop revisits its PC with the clock
   frozen. New invariant, mechanically verified on final binaries: **every
   executable cycle contains an instrumentation site** — including entry
   assembly, vDSO, usercopy, crypto, and handwritten userspace. Moments
   are named by versioned **site ID + phase**, not raw PC.
3. **Register choice corrected.** v2 nominated `x28`; arm64 Linux uses
   `x28` as the kernel `tsk` alias throughout `entry.S`, and entry/exit,
   `cpu_switch_to`, signal return, and ptrace save/replace/restore it
   [LINUX]. `-ffixed-x28` on C code does not touch any of that. Carrier
   baseline is per-vCPU always-resident memory; a register carrier is an
   optimization chosen only after auditing the exact kernel configuration
   (re-aliasing `tsk` is a small owned-kernel patch, but it is a patch,
   with a post-link verifier obligation either way).
4. **NZCV handling corrected.** v2's `cmp`-based check relied on
   pass-verified flag liveness. The sharper problem is epistemic: a
   liveness bug miscompiles *deterministically*, so dual-run comparison
   cannot see it. Default check is now NZCV-neutral
   (non-flag-setting `SUB` + `CBNZ`); the semantic oracle in the
   acceptance harness is the **uninstrumented** program's semantics, not
   another instrumented run.
5. **Carrier decision re-opened as a gate.** Round 1 pushed the deadline
   carrier off `BRK` onto HVC/SVC; round 2 argues `BRK` back in (precise,
   unmaskable, EC 0x3C with immediate in ESR, routed outward under
   `MDCR_EL2.TDE` [ARM][VERIFIED-DOC]). Resolution: the carrier is a
   day-one *measured choice* — `BRK` preferred if its gate passes
   (uniform EL0/EL1, no kernel transit), `HVC`/`SVC→HVC` the standing
   fallback. Consequence either way: with debug trapping enabled, **all**
   guest debug exceptions route to the host, and Hypervisor.framework has
   no public synchronous-exception injection — guest self-debugging
   (ptrace step, hw breakpoints, kprobes, BRK-based WARN/BUG) is forbidden
   initially or manually synthesized into guest EL1 by the VMM.
6. **kpc history corrected.** The EL-filtered leg was *not* "never run":
   commit `8501faa1` ran it as root on M4/macOS 26.5 — configurable
   `INST_BRANCH` fixed at 74 for guest loops of 0, 10⁶, and 10⁷ branches
   across EL0, EL1, and EL0|EL1 masks; `INST_ALL` fixed at 312; the fixed
   instruction counter simultaneously tracked the guest work [REPO-EVID].
   Whatever the physical EL arrangement, XNU/hvf disables or swaps
   configurable PMCs across guest entry. PMI remains untested but cannot
   rescue a guest-blind counter; probe order updated accordingly (§5.3).
7. **Portable-film contract sharpened (§3F):** identical fully
   instrumented image on both platforms; software work authoritative for
   guest-visible V-time on both; hardware PMU demoted to
   accelerator/oracle; existing uninstrumented films are not portable.
8. **Identity handling corrected:** `hv_vcpu_config_get_feature_reg` is a
   getter [SDK `hv_vcpu_config.h`] — a platform fingerprint *detects*
   incompatibility, it does not sanitize. The owned guest compiles in a
   fixed feature contract and never reads host ID registers; the VMM
   refuses record/replay on fingerprint mismatch.
9. Performance claims demoted: no ratio asserted until the full
   instrumented Linux image is measured (hand loops over-represent
   instrumentation); v1's 1.1–1.4× is withdrawn as unsubstantiated.

---

## 1. Ground rules

- **P1 (no nondeterministic results):** every guest-visible
  nondeterministic result (time, entropy, identity, counters) is trapped
  or made unreachable and replaced by a deterministic value.
- **P2 (exact async injection):** every asynchronous event is injected at
  an exact, reproducible point in guest work — never at wall-clock time.

Binding constraints: bit-identical never relaxed; cooperative guest (owned
kernel, userspace, toolchain) allowed; shipping Apple software only,
privilege posture stated per design; "unsupported" is a result.

Terminology — **film** (defined here; not established project vocabulary):
the complete durable record of one deterministic run, sufficient to
reproduce and branch it: (a) the guest image identity (hash) and platform
fingerprint; (b) the initial state (seed or snapshot); (c) the totally
ordered log of injected events, each stamped with its exact work-clock
coordinate (its Moment), plus any external input payloads. Replay
re-executes from (b) injecting (c) at the recorded coordinates; branching
replays to a Moment and diverges with a new future. Two contract
consequences used throughout: a film is bound to the exact guest image and
to the named clock unit of its coordinates (a hardware-`BR_RETIRED` film
and a `SW_EDGE_v1` film are different artifacts, never converted), and
same-seed independent runs must produce identical films. The word entered
this memo from the cross-review, which used it undefined; v2 wrongly
attributed it to the repo glossary — this session has never had the
Consonance repo, and no such attribution should be trusted. If the project
prefers "recording," substitute freely; the definition is what binds.

Established facts:

- E1. vEL2 on M4/macOS 26.5: `PMCR_EL0.N = 0`, no `PMICNTR`
  [ESTABLISHED].
- E2. Plain-EL1 exits: CANCELED / EXCEPTION / VTIMER_ACTIVATED / UNKNOWN
  [VERIFIED-DOC][SDK]; `hv_vcpus_exit` immediate kick [SDK]; `CNTVCT_EL0`
  = `mach_absolute_time() − offset`, epoch not slope [SDK][VERIFIED-DOC].
- E3 (final form). Host-side counting on M4/macOS 26.5 [REPO-EVID,
  `500f95d` + `8501faa1`, bead `hm-ssz`]: configurable counters
  guest-blind (constants 74/312 independent of guest work, across EL0 /
  EL1 / EL0|EL1 masks — the EL-filter leg *was* run); fixed counters
  guest-inclusive but contaminated (+8, +73/74, +82, rare async) and
  hardwired all-modes with no filter [XNU `monotonic_arm64.c`]; RAWPMU
  enumeration flawed in the retained probe (queried before
  `kpc_force_all_ctrs_set(1)`) — genuinely unknown, probe §5.3.
- E4. The silicon counts precisely (rr on M1/M2 under Linux, event 0x90).
  Event IDs are per-microarchitecture/database; never carried across
  M-generations without resolving from that machine's kpep database and
  logging its hash.

---

## 2. The macOS mechanism inventory

### 2.1 Hypervisor.framework, plain-EL1 guest (substrate for B, C, F)

Posture: ordinary signed process, `com.apple.security.hypervisor`
("required to use the Hypervisor APIs in any process" [VERIFIED-DOC]),
macOS 11.0+. No root, no SIP change.

| Facility | Surface | Carries | Notes |
|---|---|---|---|
| Run/exit loop | `hv_vcpu_run`; 4 exit reasons [VERIFIED-DOC][SDK] | substrate | no PMU/work exit |
| Synchronous traps | `hv_vcpu_exit_exception_t { syndrome, virtual_address, physical_address }` [VERIFIED-DOC] | **P2**: doorbells, MMIO, fault-driven dirty tracking | `HVC` → `EC_AA64_HVC` (PSCI conduit precedent) [CITED: QEMU hvf]; `BRK` → EC 0x3C, immediate in ESR, precise and unmaskable [ARM], routed outward under debug trapping ≙ `MDCR_EL2.TDE` [VERIFIED-DOC]; both carriers gated day-one [M-18, M-22] |
| Debug traps | `hv_vcpu_set_trap_debug_exceptions` [VERIFIED-DOC]; `MDSCR_EL1`, `DBGB/W{V,C}R0–15_EL1`, `SPSR_EL1`, `ELR_EL1` [VERIFIED-DOC] | B0 carrier candidate (`BRK`); B1 stepping; debug workflows | **all** guest debug exceptions route outward when enabled; no public synchronous-exception injection ⇒ guest self-debug forbidden or VMM-synthesized (§3B); no documented one-retired-instruction contract [SDK] ⇒ gate M-03 |
| Interrupts | `hv_vcpu_set_pending_interrupt` [VERIFIED-DOC]; pending cleared after each run [SDK] | last-resort delivery only | baseline is synchronous PV dispatch at stops; the hardware pin is gated on acceptance-point determinism [M-20] and never left pending across instrumentation |
| Timer | `hv_vcpu_set_vtimer_offset`/`_mask` [VERIFIED-DOC] | nothing deterministic | epoch only; VTIMER exits at most invisible pacing stops |
| Guest counter control | `HV_SYS_REG_CNTKCTL_EL1` [VERIFIED-DOC] | **P1** | owned kernel traps guest-EL0 counter reads to itself |
| PMU access | no PMU regs in `hv_sys_reg_t` [VERIFIED-DOC]; PMU-access trap config in macOS 26 `hv_vm_config.h` [SDK] | **P1** | configure trap + audit absence [M-04] |
| Memory | `hv_vm_map/unmap/protect` [VERIFIED-DOC] | snapshot; SW dirty log via write-protect faults | no dirty log — speed, not correctness [M-19] |
| Kick | `hv_vcpus_exit` → CANCELED | stop-look-resume only | invisibility gate [M-11]; never an injection point |
| GIC | `hv_gic_create` [VERIFIED-DOC]; opaque state, restore may fail across versions [SDK `hv_gic_state.h`] | — | unused; PV event controller instead |
| ID/feature regs | `hv_vcpu_config_get_feature_reg` — **getter** [SDK `hv_vcpu_config.h`] | fingerprint = refusal check only | guest never reads host ID regs; fixed feature contract compiled in |

### 2.2 Virtual EL2 (macOS 15+, M3+)

`hv_vm_config_set_el2_enabled` [VERIFIED-DOC]. No PMU behind it (E1);
multiplied trap cost [M-05]; youngest emulation surface. Closed except as
a possible C-accelerator (§3C) or if Apple ships a nested PMU (§6).

### 2.3 Host performance counters — measured, negative

Full statement in E3. Consequences: no EL mask, no root posture, and no
private framework makes the configurable bank see the guest on the tested
system; the guest-inclusive fixed bank cannot be filtered by any current
configuration [XNU]; kpc privilege is root/blessed-PID with the
entitlement bypass compiled for development kernels [XNU];
`thread_selfcounts` is a private read-only unfilterable SPI
[XNU `resource_private.h`] — heuristic only. Sole remaining question:
RAWPMU after `kpc_force_all_ctrs_set(1)` (§5.3).

### 2.4 Structural absences

No `perf_event_open` analogue; no per-thread PMI delivery contract; no
work-deadline run; no dirty log; no guest PMU; no counter-slope control;
no exact single-step retirement contract; no synchronous-exception
injection; Virtualization.framework has no vCPU surface.

---

## 3. Alternatives

Tiers: **T0** ≤1.4×, **T1** 1.4–3×, **T2** 3–100×, **T3** >10³×.
Planning envelopes, not measurements.

### B. Guest software work clock (`SW_EDGE_v1`) — primary, in two stages

The inversion: stop asking the host to observe guest work; make the guest
carry its clock as architectural state. macOS then only has to run the
guest, deliver synchronous exits precisely, and expose guest state at
stops — all verified public surface (§2.1).

**Unit and Moments.** `SW_EDGE_v1` = executed validated instrumentation
sites. A **Moment** (event coordinate in a film) is `(work, site_id,
phase)` — versioned site IDs from the build, not raw PCs. V-time =
f(work), per `docs/PARAVIRT-CLOCK.md`, software-authoritative (§3F).

**Site coverage invariant (mechanically verified on final binaries):**
every executable cycle contains a site — compiled code by the pass;
handwritten assembly (entry paths, vDSO, usercopy, crypto, string
routines) by hand-placed sites in every loop; anything unverifiable is
rejected at image assembly. This is what makes Moments unique and replay
landings well-defined; v2's allowance of site-free assembly loops was an
error (§0.1-2).

#### B0 — correctness baseline

Per-chunk check, NZCV-neutral, `BRK`-carried (carrier gate below):

```asm
    sub     xB, xB, #1        // budget decrement; no flags touched
    cbnz    xB, 1f            // budget remaining: continue
    brk     #0x5A5A           // authoritative stop, BEFORE the chunk's
1:                            //   first original instruction
    ; original chunk instructions (statically bounded length)
```

- Budget `xB` lives in always-resident per-vCPU memory in the first
  implementation (load/decrement/store must then be single-copy atomic —
  LSE `ldadd`-class — so interposed handlers cannot be erased); a
  register-resident budget is adopted only after the kernel audit (§0.1-3:
  not `x28`; arm64 Linux aliases it as `tsk` in `entry.S` [LINUX];
  whichever register is chosen, entry/exit, `cpu_switch_to`, signal
  return, `ptrace`, and `setcontext` paths are patched and a post-link
  verifier enforces that nothing else writes it).
- Zero fires **before** any original instruction of the chunk executes:
  injection points are exact chunk boundaries by construction. No
  single-step on the correctness path; no sparse-gap reasoning; no
  flag-liveness proof (the sequence is NZCV-neutral; no-wrap holds
  because the host installs budgets ≥ 1 and only sites decrement).
- **Carrier gate [M-22]:** `BRK #imm` from EL0 and EL1, ≥10⁶ reps under
  host load — require EXCEPTION exit, EC 0x3C, correct immediate and PC,
  no guest-vector execution, exact one-time continuation after the VMM
  advances PC. If `BRK` fails the gate, the fallback carrier is: EL1
  branches to an `HVC` thunk; EL0 executes `SVC` into a patched
  IRQ-masked EL1 trampoline that immediately `HVC`s [M-18 syndrome
  confirmation]. The clock design is carrier-independent.
- **Debug-channel reservation (consequence of trapping):** all guest
  debug exceptions route to the host; there is no public way to inject a
  synchronous exception back. Initially the owned guest simply has no
  self-debug: no kprobes, no ptrace single-step, no hw breakpoints,
  WARN/BUG via a non-BRK mechanism or reserved immediates. If guest-side
  debug is ever wanted, the VMM synthesizes the exception manually
  (write `ELR_EL1`/`SPSR_EL1`/`ESR_EL1`, redirect PC to the vector) —
  deterministic, but it is built machinery, not a free behavior.
- **Event delivery:** synchronous PV dispatch is the baseline — at a
  stop, the VMM posts events to the shared event page; the doorbell
  return path (or the EL1 trampoline, for EL0 stops) invokes the kernel's
  dispatcher at that exact boundary, honoring the kernel's soft-mask
  state. Hardware IRQ-pin injection is last-resort, gated on
  acceptance-point determinism [M-20], and never left pending across
  instrumentation sequences.

#### B1 — optimized sparse guards + exact landing (only after B0 passes)

1. Absolute site counter (`work`) incremented at every site; sparse
   **guard** sites carry the budget check.
2. Guard graph with a **mechanically proven maximum gap K** (sites
   between consecutive guards on every path): every CFG cycle contains a
   guard; function entries guard call paths (covering indirect calls and
   recursion); exception vectors and landing pads guard exceptional
   paths; long acyclic runs get interior guards.
3. To reach target W: arm so that **every possible first `BRK` satisfies
   `work < W`** (arm at `W − K` against the proven gap), and the host
   *asserts* `work < W` at the stop — a violated assertion is a verifier
   bug and halts, never a silent overshoot.
4. Qualified single-step from there to the canonical boundary with
   `work == W`; inject before the first original instruction.
5. Step qualification [M-03] must pass first: ≥10⁶ steps across faults,
   SVC/HVC, ERET, pending events, idle, LSE (and demonstrate the LL/SC
   hazard to justify its exclusion); exactly one retirement or a
   precisely classified non-retirement per exit; one unexplained step
   kills B1 — **B0 survives**.

B1 exists because B0's per-chunk check is the dominant overhead; sparse
guards move the steady-state cost to (rare) guard checks plus a bounded
stepping phase at landings. Record-mode injection can also run pure-B0
style (stop at the first guard past W and *define* the Moment there —
quantized injection, sound and cheap); exact-W landing is required for
replaying arbitrary recorded Moments and for debug/branch workflows.

#### Shared P1 inventory (both stages)

Guest-EL0 counter reads trapped in-guest via `CNTKCTL_EL1.EL0{V,P}CTEN=0`
[VERIFIED-DOC reg]; kernel-side counter reads unreachable (no arch-timer
clocksource) + link-time encoding audit (`CNTVCT/CNTVCTSS/CNTPCT`, `RNDR`,
`LDXR/STXR`, raw `WFI/WFE`, PMU regs) over every image; vDSO serves
V-time (= f(work), readable in-guest); PMU access configured to trap
[SDK][M-04] and absent by audit; entropy via virtio-rng from the seeded
monitor; **identity**: fixed feature contract compiled into the guest —
it never reads host ID registers; the VMM records the platform
fingerprint (`hv_vcpu_config_get_feature_reg` — getter only [SDK]) into
films and refuses replay on mismatch; PAC disabled; WFI/WFE compiled out
into deterministic idle hypercalls (V-time warps only by the deterministic
next-deadline policy); alternatives/static-keys/text-patching pinned at
build; modules, BPF JIT, userspace JIT, and W+X mappings disabled;
LSE-only atomics.

#### Snapshot / restore / branch

At a stop: full RAM; all public CPU/system/SIMD-FP/debug registers; clock
state (work, budget, armed deadline); pending-event latches; device
state; input-log cursor; canonical hash. Dirty tracking via
`hv_vm_protect` write-protect faults later [M-19]. Branching = restore +
different injected future (B is record-capable, so branching works on
macOS).

#### What this demands of the system under test

Stated plainly, because it is the tier's real product constraint: unlike
the hardware-clock backends — which need only the owned kernel and can run
arbitrary userspace binaries under it — this tier requires **every
executable component** of the guest (kernel, libc, every binary and
library) to be produced by the instrumenting toolchain, or to pass the
same post-link verifier. Three boundaries soften it: data is untouched
(files, databases, network inputs, configuration — no rebuild);
interpreted and bytecode programs are data once their runtime is
instrumented and JIT-less (an instrumented CPython runs uninstrumented
Python scripts; same for shells); and inputs to the workload are always
free. What cannot run on this tier initially is binary-only software with
no rebuild path — its routes are the E1 emulation tier today, an
instrumented in-guest emulator, or a future load-time binary rewriter
sitting behind the same verifier. Note the regime was already
image-bound: films bind to an exact image hash on every backend; the new
demand is that this image be *ours to build*, not merely ours to hash.

#### Posture, tier, risks, fragility

Hardware floor: **any Apple-silicon generation, M1 up** — B runs the
guest at plain EL1 and uses no nested-virtualization feature. The
M3+/macOS 15 requirement belonged to the dead virtual-EL2 program and
survives only in closed or optional paths (D; the vEL2-accelerated
stepping variant of C; a hypothetical future nested PMU). The API surface
B uses dates to macOS 11.0; the practical OS floor is whichever releases
the determinism gate is qualified on (the macOS 26 PMU-trap knob is
optional hardening, not a dependency). Films stay platform-fingerprint-
bound; cross-chip replay is plausible precisely because the guest never
consults host identity, but remains refused until demonstrated.

Entitlement only; no root/SIP. Tier: research path until M-01 measures
the **full instrumented Linux image** (hand loops over-represent
instrumentation); envelope 1.2–5×, demote-don't-relax. Risks: hvf-side
guest-visible nondeterminism (kill condition, §5.1); carrier-gate
failure (falls back HVC/SVC); closure gaps (fail-closed by verifier +
negative tests); semantic miscompilation by the pass (caught by the
uninstrumented-oracle gate, §5.1 — *not* by dual-run, which identical
wrongness defeats). Fragility low-to-medium: public documented API;
requalify per macOS update with the automated gate.

### E. Deterministic emulation

**E1 — x86-64 TCG product fallback.** Existing x86-64 guest and machine
contract under single-threaded TCG on macOS/arm64 [CITED: QEMU docs].
Stock `icount` is machinery, not the clock (TB budgets; MTTCG-
incompatible) [CITED]; stock record/replay records host clocks as inputs
and is insufficient for same-seed independent runs [CITED]. Clock: either
a new `TCG_INSN_v1` coordinate, or preferably a translator extension
decrementing only on instructions satisfying the pinned
`BR_INST_RETIRED.CONDITIONAL` contract, counting after retirement,
forcing an outer-loop exit before the next instruction at zero; event-
class boundary (`Jcc`, `LOOP*`, `JCXZ/JECXZ/JRCXZ`, fusion, faulting
cases, errata) differentially validated against the Intel/KVM backend —
one unexplained mismatch kills *film compatibility* (a fresh coordinate
may proceed). Closure: emulator-supplied TSC/CPUID/MSR/RNG; virtual
clocks from branch work; no host RTC/RNG/audio/passthrough; one vCPU or
serialized schedule; pinned QEMU/machine/CPU/build/devices; no host-time
idle warp. Posture: plain process; JIT needs
`com.apple.security.cs.allow-jit` + `MAP_JIT` [VERIFIED-DOC-class];
interpreter avoids both at higher cost. Tier T2 (10–100×; >100× ⇒
oracle). Lowest Apple fragility of any option.

**E2 — arm64 TCG icount oracle** for B payloads: third opinion in
divergence triage; forensic re-execution while M-03 is unproven; unit
named `TCG_INSN`-class, never conflated with `SW_EDGE_v1`.

### C. Debug-architecture-only execution

Step-per-instruction with host classification. Public surface exists; the
missing piece is a documented exact retirement contract — gate M-03 (see
B1 step 5). Direct host stepping ~10³–10⁵×; a vEL2-hosted variant (debug
exceptions handled at virtual EL2 without leaving the VM) might reach
~10²–10⁴× at nested-virt fragility [M-05] — both unmeasured. Role:
validation oracle, slow replay engine, B1's landing primitive. LL/SC
unsafe under stepping — LSE-only guests. Breakpoints identify a PC, not
its k-th dynamic visit: accelerators only, never position authority.

### A. Host kpc/kperf — closed; one gated half-day probe

Measured NO-GO (E3). Remaining probe, strictly ordered: (1) record
model/build/UID/signature/entitlements/`csrutil status`/event-DB hash/
core type; (2) `kpc_force_all_ctrs_set(1)` and require readback 1
**before** enumerating FIXED/CONFIGURABLE/POWER/RAWPMU (the retained H3b
enumerated first — flaw, not finding); (3) **only if** RAWPMU then
demonstrates exact guest-only counting (host positive control, then guest
loops N ∈ {0, 10³, 10⁶, 10⁷} with Δ=N over ≥10⁶ windows under load) do
period/overflow tests proceed: overflow must return `hv_vcpu_run` to
userspace before the guest passes the target (in-kernel sample with
transparent re-entry is useless); measure loss/duplication/skid; prove
early-arm + landing never overshoots. The ARM kpc reload path appears
configurable-only [XNU] — a generic `kpc_set_period` success is not
fixed-PMI evidence. Kill on any failure; B and E1 proceed regardless.

### D. vEL2 monitor without a PMU — closed by construction

A monitor counts only what it observes; a trap-free loop passes any
deadline. Viable completions reduce to B (per-checkpoint HVC at higher
cost) or C (stepping, possibly vEL2-accelerated). Re-open only on a
nested PMU (§6).

### F. Replay-only macOS — folded, with a sharpened contract

Replay still needs "when is recorded position W reached?" — a position
mechanism (B, E, or C). For **cross-platform portable films** the
contract is: identical fully instrumented image on both platforms;
Moments as versioned `(work, site_id, phase)`; **software work
authoritative for guest-visible V-time on both platforms** — otherwise
V-time values materialized from a hardware clock on the record side are
unreproducible on macOS without logging every materialization; the Linux
PMU serves as accelerator/oracle, not as a guest-visible clock.
Instrumentation changes hardware counts (checks are retired branches;
register reservation shifts spills and control flow), so instrumented-
image hardware baselines are remeasured and **existing uninstrumented
films are not portable** — replaying those requires E1 (x86) or C-class
decoding, or a heavyweight per-event cross-index record. Branching into
new futures on macOS additionally requires a record-capable clock: B.

### G. Adjacent, not macOS: Linux bare metal (Asahi lineage)

Would validate the physical PMU event and KVM guest-only counting on
M-cores — none of the macOS surface. Not ready: M3 PMU TBA / installers
WIP; M4 PMU TBA / installers unavailable [CITED]. Gate when testable:
the same 10⁶-window branch/PMI/skid/step experiment with the event
resolved from that machine's database (0x90 is an M1/M2 fact, E4).

### H. Non-starters

Virtualization.framework (no vCPU surface); unmodified-hvf wall-clock
injection (violates P2); kexts / SIP-off / private-framework postures.

---

## 4. Ranking

| Rank | Design | Tier (planning) | Verdict |
|---:|---|---|---|
| 1 | **B0 → B1** — `SW_EDGE_v1` on direct-EL1 HVF | research path; envelope 1.2–5× | best product candidate; B0 first, B1 only after |
| 2 | **E1** — x86-64 TCG, exact translated branch clock | 10–100× | shipping fallback + only path for existing films; start in parallel |
| 3 | **E2** — arm64 TCG oracle | oracle | adopt, no spike |
| 4 | **C** — debug stepping | 10²–10⁵× | oracle / B1 primitive behind M-03 |
| 5 | **A** — kpc/kperf | n/a | NO-GO; gated half-day RAWPMU probe |
| 6 | **F** — replay-only | derivative | folds into B/E/C |
| 7 | **D** — vEL2 sans PMU | n/a | closed by construction |
| — | **G** — Linux/Asahi | native-class, not macOS | tracked; M3/M4 PMU TBA |

Risk shape: B's risks are our engineering (closure, pass correctness,
kernel patches) plus two narrow Apple gates (carrier, CANCELED
invisibility); E1's risks are translator fidelity — ours; everything
below depends on Apple behavior that is absent, undocumented, or measured
hostile.

## 5. Spike specifications

### 5.1 Spike B — the gate ladder (in order; each gate falsifiable)

1. **Carrier gate [M-22/M-18].** `BRK #0x5A5A` from EL0 and EL1, ≥10⁶
   reps under load: EXCEPTION exit, EC 0x3C, correct immediate + PC, no
   guest-vector execution, exact one-time continuation. Same battery for
   the `HVC` and `SVC→HVC` fallback (syndrome, marshalling). Pick the
   carrier on evidence.
2. **Semantic-preservation gate.** Instrumented vs **uninstrumented
   semantic oracle** (independent interpreter or uninstrumented native
   run of the same sources): NZCV-live code, syscalls, faults, ERET,
   signals, task switches, `rt_sigreturn`, `setcontext`, fork/exec, host
   cancellations. The pass must change no architectural result other
   than the clock. Dual-run comparison explicitly does not count here —
   identical wrongness passes it.
3. **B0 exact-clock gate.** ≥10⁶ randomized budgets over the §5.1
   payload (direct/conditional/indirect flow, calls/returns, repeated
   PCs, SVC/HVC/ERET, faults, masked/unmasked events, LSE, deterministic
   idle, CANCELED storms [M-11]): zero overshoots, zero skipped traps,
   zero counter rollbacks, zero canonical-boundary mismatches; exactly
   one injection per target, none early or late; identical full-state
   hashes across fresh processes and load; ≥10⁴ snapshot/restore
   round-trips with identical suffixes; negative tests fail closed
   (site-free cycle, forbidden encoding, budget-state clobber, W+X page,
   raw `WFI` — each must be rejected or trip the harness).
4. **B1 gates (only after 1–3 pass).** Guard-graph verifier proves K;
   host asserts `work < W` at every armed first-stop (violation = halt);
   step qualification [M-03] as §3B1(5). One unexplained step kills B1,
   not B0.
5. **Performance [M-01].** Full instrumented Linux image (kernel boot +
   userspace workload mix), not hand loops; report median/p99 vs
   uninstrumented hvf. >~5× median / ~10× p99 demotes the tier.

Kill condition for B0 (and with it the fast path): any guest-visible
divergence between identically seeded runs attributable to hvf itself
that construction cannot make unreachable; or carrier-gate failure of
*both* carriers; or demonstrated inability to enforce closure
(fail-closed becomes fail-open anywhere).

### 5.2 Spike E1 — slow path first

As v2: branch-budget slow path only; ≥10⁶ randomized deadlines; compare
to an independent x86 decoder/interpreter; differential vs Intel/KVM
backend; exit after target branch, before next instruction; full-state
hashes across fresh processes; ≥10⁴ restores; perturb host RTC/load/
ordering with unchanged guest state. One unexplained mismatch kills film
compatibility (fresh coordinate may proceed); >~100× reclassifies to
oracle.

### 5.3 Probe A — half day, gated, only on an already-rooted M4

Exactly §3A's ordered list. Outcome cannot change the ranking — it can
only add a lab accelerator or close the file.

### 5.4 Standing oracle E2

§5.1 payload under `qemu-system-aarch64 -accel tcg -icount
shift=…,sleep=off`; identical architectural trajectory; wired into CI.

## 6. What Apple would have to ship

**Exists, gated/private or insufficient:** kpc configurable counting +
kpep DBs (private, root-owned, and guest-blind as tested — opening access
alone would not suffice; Apple must also attribute guest events and
deliver overflow to the owning VMM); kpc period/action (no vCPU-return
delivery); RAWPMU (private/root; corrected enumeration unmeasured; no
fixed-`PMCR1` control); `thread_selfcounts` (private, read-only,
unfilterable); debug trapping (no exact-retirement contract, no
synchronous-exception injection); GIC opaque state (restore may fail
across versions); `hv_vm_protect` (accelerator, not a dirty log).

**Exists in silicon, absent in software:** precise branch counting with
any guest visibility (plain-EL1: no PMU; vEL2: `PMCR_EL0.N = 0`);
FEAT_ECV-class guest counter trapping (silicon presence unknown, M-16).

**Does not exist (would have to be designed):** per-vCPU guest-only work
counter with a stable named-event ABI; host/guest + EL filtering defined
to include hvf guest execution; absolute counter deadline / run-until-N;
a dedicated work-counter/PMU-overflow exit reason; documented PMI skid
and delivery bounds; public counter+overflow state save/restore; nonzero
nested PMU bank; documented exact single-instruction execution primitive
across exceptions/IRQs/idle; synchronous-exception injection into guests;
virtual-counter slope ownership; public dirty log (performance only).

Minimal reviving API: configure a stable event + guest privilege mask;
read/write/serialize the count; arm an absolute target; return the vCPU
with a dedicated exit reason before the next guest instruction. With the
existing debug trap, that makes the proven overflow-and-step design
credible on macOS.

## 7. Measurement ledger

Resolved:

| # | Question | Resolution |
|---|---|---|
| M-06 | kpc EL masks give guest counting | **Negative, measured** — guest-blind across EL0/EL1/EL0\|EL1 [REPO-EVID `8501faa1`] |
| M-08/M-10 | host-EL0 constant / thread attribution | mooted by M-06 |
| M-17 | Asahi M3/M4 KVM+PMU readiness | **not ready** (PMU TBA) [CITED] |

Open:

| # | Question | Blocking? |
|---|---|---|
| M-01 | B overhead on **full instrumented Linux image** (median/p99) | tier claim |
| M-02 | WFI-exit behavior (backstop; baseline compiles WFI out) | no |
| M-03 | Step exactness: one retirement or classified non-retirement per exit, across faults/SVC/HVC/ERET/pending/idle/LSE | gates **B1** and C only |
| M-04 | PMU-access trap config behavior + guest PMU read semantics | P1 audit |
| M-05 | vEL2 nested trap cost (C-accelerator only) | no |
| M-07 | Probe-A environment (SIP, Developer Mode) | probe only |
| M-09 | kpc PMI delivery/skid (probe, only past RAWPMU gate) | probe only |
| M-11 | CANCELED stop/resume invisibility under storms | **B0 kill-relevant** |
| M-12 | `ID_AA64ISAR0.RNDR` under hvf (audit-forbidden regardless) | P1 audit |
| M-13 | MIDR/MPIDR writability via `hv_vcpu_set_sys_reg` (MPIDR settable per GIC doc [VERIFIED-DOC]; MIDR unknown) | nicety — identity handled by fixed contract regardless |
| M-14 | Snapshot state-completeness round-trip | B milestone 2 |
| M-15 | hvf exit round-trip costs (BRK, HVC, SVC→HVC, MMIO, step) | tier math |
| M-16 | FEAT_ECV presence | no |
| M-18 | HVC/SVC syndrome path confirmation (EC, ISS, marshalling) | **B0 day-one** (fallback carrier) |
| M-19 | `hv_vm_protect` write-fault behavior for dirty tracking | speed only |
| M-20 | Hardware IRQ-pin acceptance-point determinism (pin is last-resort; synchronous PV dispatch is baseline) | only if pin used |
| M-21 | Budget carrier: audited-register vs atomic-memory (cost + hazard evidence; register choice post kernel-audit, not `x28`) | B0 day-one |
| M-22 | **BRK carrier gate**: EC 0x3C, immediate, PC, no guest-vector execution, exact continuation, EL0+EL1, ≥10⁶ under load | **B0 day-one** |

## 8. Citations

Apple documentation 2026-08-06 [VERIFIED-DOC]: `hv_vm_config_set_el2_enabled`
(15.0+); `hv_vcpu_set_trap_debug_exceptions` (11.0+, "debug exceptions
exit the guest", ≙ `MDCR_EL2.TDE`); `hv_exit_reason_t`;
`hv_vcpu_exit_exception_t`; `hv_vcpu_set_vtimer_offset` (≙ `CNTVOFF_EL2`);
`hv_vcpu_set_vtimer_mask`; `hv_vcpu_set_pending_interrupt`;
`hv_vm_map`/`hv_vm_protect`; `hv_sys_reg_t` (MDSCR, DBGB/W 0–15, CNTKCTL,
SPSR/ELR_EL1; no PMU regs); `hv_vcpu_config_get_feature_reg`;
`hv_gic_create`; entitlements `com.apple.security.hypervisor`,
`com.apple.security.cs.allow-jit`.

macOS 26.4.1 SDK [SDK, via cross-review; re-verify in CI]:
`hv_vcpu_types.h`; `hv_vcpu.h` (kick semantics; `CNTVCT =
mach_absolute_time() − offset`; per-run pending-interrupt clearing; debug
traps); `hv_vm.h`; `hv_vm_config.h` (PMU-access trap); `hv_gic_state.h`
(opaque state, restore may fail); `hv_vcpu_config.h`
(`hv_vcpu_config_get_feature_reg` is a getter).

Arm architecture [ARM]: `BRK` — precise, unmaskable Breakpoint
Instruction exception, EC 0x3C, immediate in ESR ISS; debug-exception
routing under `MDCR_EL2.TDE` (Armv8-A self-hosted debug guide).

Linux mainline [LINUX]: `arch/arm64/kernel/entry.S` (`tsk` ≡ `x28`
alias; entry/exit register handling), `arch/arm64/kernel/signal.c`
(signal-frame register restore) — basis for the register-carrier audit.

XNU at `f6217f89…` [XNU]: `osfmk/arm64/monotonic_arm64.c` (fixed
counters all-modes, unfiltered); `osfmk/arm64/kpc.c` (RAWPMU register
list without fixed `PMCR1`; configurable-only PMI reload);
`bsd/kern/kern_kpc.c`, `osfmk/kperf/kperfbsd.c`, `bsd/kern/kern_ktrace.c`
(root/blessed-PID/dev-kernel gating); `bsd/sys/resource_private.h`
(`thread_selfcounts`).

Consonance repo [REPO-EVID]: `spike/as2h-host-count` @ `500f95d`,
`8501faa1` (H3: configurable guest-blind across EL masks and 0/10⁶/10⁷
loops; fixed-counter contamination; H3b RAWPMU ordering flaw); bead
`hm-ssz`; `docs/APPLE-SILICON.md`; `docs/PARAVIRT-CLOCK.md`;
`docs/GLOSSARY.md`; `docs/MACOS-M3-BACKEND-ALTERNATIVES.md` (breakpoint+
short-step proposals: accelerators only, superseded); bead `hm-dj0`.

Third-party [CITED]: QEMU `target/arm/hvf/hvf.c` (`EC_AA64_HVC` conduit,
WFx, sysreg save/restore); QEMU hvf gdbstub series (Cagnin, merged 8.x —
debug traps exercised in shipping software; exactness ours to prove);
QEMU build-platforms / emulation / icount / record-replay docs (MTTCG
incompatibility; host clocks recorded as inputs); FFmpeg
`libavutil/macos_kperf.c`; rr on M1/M2 under Linux (event 0x90); Asahi
M3/M4 feature-support tables.

## 9. Cross-review adjudication

### Round 1 (v1 → v2) — summary

Accepted: kpc architecture NO-GO [REPO-EVID]; carrier off the debug
channel (superseded in round 2 by the carrier gate); LL/SC exclusion;
pending-IRQ latch [SDK]; WFI/WFE compiled out; E1 as x86-64 TCG with
film-compat obligations; fail-closed negative tests; demote-don't-relax;
wishlist upgrades. Rejected/corrected: non-atomic memory countdown
(single-copy-atomic required); breakpoint critique correct in general but
not against a clock-authoritative landing rule.

### Round 2 (v2 → v3)

Accepted, with the evidence that compelled each:

- **R12. Site-coverage invariant.** v2's (W, PC)-uniqueness claim was
  false under uninstrumented assembly loops. Adopted: every executable
  cycle contains a site, mechanically verified; Moments are
  `(work, site_id, phase)`.
- **R13. `x28` disqualified.** arm64 Linux aliases `x28` as `tsk` across
  `entry.S`; `-ffixed-x28` is insufficient by itself [LINUX]. Register
  carrier only after kernel audit; memory budget is the baseline.
- **R14. NZCV-neutral default + uninstrumented oracle.** Liveness-based
  `cmp` is not unsound, but a liveness bug miscompiles deterministically
  and dual-run comparison cannot detect it. Default sequence is
  `sub`/`cbnz`/`brk`; the semantic gate compares against uninstrumented
  semantics.
- **R15. B0/B1 split** with the arm-early invariant (`work < W` at every
  first stop, host-asserted) and the guard-graph proof obligation (K over
  acyclic, interprocedural, recursive, and exceptional paths — backedges
  alone do not bound it).
- **R16. kpc history corrected**: the EL-filter leg was run (`8501faa1`)
  and is guest-blind across masks; "finish the never-run leg" withdrawn.
  Probe re-ordered: RAWPMU must demonstrate guest counting before any
  PMI test.
- **R17. Debug-channel reservation**: with trapping on, *all* guest
  debug exceptions exit, and no public synchronous-exception injection
  exists — guest self-debug forbidden initially or VMM-synthesized.
- **R18. Identity**: `hv_vcpu_config_get_feature_reg` is a getter [SDK];
  fingerprints refuse, they do not sanitize; the guest compiles in a
  fixed feature contract and never reads host ID registers.
- **R19. Portable films need software-authoritative V-time on both
  platforms**; hardware PMU demoted to accelerator/oracle; uninstrumented
  films are not portable.
- **R20. Performance honesty**: no ratio until the full instrumented
  image is measured; v1's 1.1–1.4× withdrawn.

Corrected back (for the record, without changing the adopted outcome):

- **R21.** Round 2's "blocking flaw 1" reviews an injection rule v1/v2
  did not propose: injection was specified as *quantized* — "the first
  check site where x28 ≥ W," a pure function of guest state, identical
  on record and replay — which is round 2's own sound formulation (2);
  exact-W landing via arm-early + step was already the stated mechanism
  for arbitrary recorded points (formulation 3). What round 2 genuinely
  adds — the mechanized guard-gap proof and the host-checked `work < W`
  assertion — is adopted in B1 regardless.
- **R22.** Round 2's carrier position reverses round 1's: round 1 argued
  the deadline carrier off `BRK` (undocumented debug-delivery contract);
  round 2 argues `BRK` in (precise, unmaskable, EC 0x3C [ARM]). Neither
  argument is dispositive without hardware: the carrier is now a day-one
  gate [M-22] with `HVC`/`SVC→HVC` as the standing fallback, and the
  clock design is carrier-independent.

Standing convergence across all three reviews: software work counting on
public direct-EL1 Hypervisor.framework is the only credible fast path on
macOS; deterministic TCG is the independent fallback; every
hardware-assisted path fails on the same missing property — no Apple
surface attributes, filters, or delivers guest work counts to the VMM.
