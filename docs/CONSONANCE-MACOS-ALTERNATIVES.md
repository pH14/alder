# Consonance on macOS / Apple Silicon (M3+): Alternatives Design Memo

Status: design memo, for review. Written 2026-08-06.

Provenance: this memo was written without the Consonance repo checked out; it
builds on the prior program's findings as summarized in the task brief (the
dead virtual-EL2 program of `docs/APPLE-SILICON.md`, the host-side counting
spike, and the open design threads in beads `hm-ssz` / `hm-dj0`). Facts from
that program are labeled [ESTABLISHED] and are not re-derived here. Every
claim about an Apple API surface was checked against Apple's developer
documentation on 2026-08-06 and is labeled [VERIFIED-DOC], with the citation
in §8. Claims resting on third-party source code in shipping projects are
labeled [CITED]. Anything that can only be settled on hardware is labeled
[MEASURE M-nn] and collected in the ledger in §7. Nothing below silently
substitutes a mechanism for another: where a design names a counter or an
event, that exact counter or event is meant.

---

## 0. Verdict (TL;DR)

There is a credible fast path, and it does not depend on any Apple PMU
surface at all.

1. **Primary recommendation — Alternative B, "guest software work clock":**
   put the work clock *inside the guest* as architectural state. Our owned
   toolchain reserves two GPRs (`x28` = work counter, `x27` = threshold),
   inserts a counter increment at every compiled basic block and a
   two-instruction threshold check at every loop backedge; crossing the
   threshold executes `BRK`, which — with
   `hv_vcpu_set_trap_debug_exceptions(true)` [VERIFIED-DOC] — exits to the
   host at an exact, reproducible, overshoot-impossible point. Async
   injection happens only at these check sites and at kernel doorbell
   hypercalls (`HVC` exits to the VMM [CITED: QEMU hvf]). V-time stays a pure
   function of the work count, exactly as in `docs/PARAVIRT-CLOCK.md`, with
   the unit redefined from "retired conditional branches" to "executed
   instrumentation sites." Public API only, no root, no private frameworks,
   `com.apple.security.hypervisor` entitlement only [VERIFIED-DOC].
   Estimated tier: 1.1–1.4× native-virtualization speed [MEASURE M-01].
   Day-one experiment is ~300 lines and falsifiable in §5.1.

2. **Secondary, bounded — Alternative A, finish the kpc spike on a root lab
   box.** Not as the product path (private API, root-gated, fragile) but
   because it (a) resolves whether the M4 +73/74 contamination is excludable
   by EL filtering, (b) gives an independent, uninstrumented oracle to
   cross-check B's counts, and (c) keeps an uninstrumented-guest lab tier
   alive. 2–3 days on hardware. §5.2.

3. **Standing floor — Alternative E (QEMU TCG icount)** is adopted as the
   deterministic slow oracle for differential testing, not spiked: it is
   known technology, ~10–50× slowdown, zero Apple-API risk.

Everything else: the vEL2-without-PMU monitor (D) is deferred as an optional
later hardening layer, not a work-clock source; a replay-only tier (F)
collapses into B once records log the guest software clock alongside the
hardware clock on Linux backends; debug-architecture-only execution (C)
survives as B's landing primitive and as a last-resort tier at 10³–10⁴×;
bare-metal Linux on Apple Silicon (G) is named honestly as leaving macOS and
as M1/M2-only today.

The honest blocker list for anything faster than B without instrumentation is
short and specific (§6): Apple ships no guest PMU (silicon has one; the vEL2
emulation advertises `PMCR_EL0.N = 0` [ESTABLISHED]), no run-until-work-count
primitive, no public per-thread PMI, and no dirty-page log.

---

## 1. Ground rules

The two properties any design must carry, verbatim from the program:

- **P1 (no nondeterministic results):** every guest-visible nondeterministic
  result (time, entropy, identity, counters) is trapped or made unreachable
  and replaced by a deterministic value.
- **P2 (exact async injection):** every asynchronous event is injected at an
  exact, reproducible point in guest work — never at wall-clock time.

Binding constraints honored throughout: bit-identical is never relaxed; a
cooperative guest (owned kernel, owned userspace, owned toolchain) is
allowed; shipping Apple software only, with the exact privilege posture
stated per design; "unsupported" is a result.

Working assumptions inherited as [ESTABLISHED] (from `docs/APPLE-SILICON.md`
and the spike, per the brief; none re-derived here):

- E1. vEL2 on M4/macOS 26.5 advertises `PMCR_EL0.N = 0`, no `PMICNTR`: no
  nested hardware work clock exists.
- E2. Plain-EL1 Hypervisor.framework exit vocabulary is
  CANCELED / EXCEPTION / VTIMER_ACTIVATED / UNKNOWN (re-verified against
  Apple docs [VERIFIED-DOC]); the guest virtual counter is host
  `mach_absolute_time` minus an offset (epoch movable, slope not);
  `hv_vcpus_exit` is an immediate kick; no `perf_event_open` analogue; no
  public dirty log.
- E3. `thread_selfcounts`-style fixed host-thread instruction counters were
  floor-exact for guest EL1 work on M1 Max and M4, except an M4-only
  quantized +73/74-instruction kernel-side contamination at some window
  sizes; EL-filtered counting and `kpc_set_period` PMI were never run
  (blocked on root).
- E4. The silicon counts precisely (rr on M1/M2 under Linux, Apple event
  0x90 taken-branches). The gap is Apple's software surface.

---

## 2. The macOS mechanism inventory

What shipping macOS actually offers each property. Availability is from
Apple's documentation, fetched 2026-08-06 (§8).

### 2.1 Hypervisor.framework, plain-EL1 guest (the substrate for B, C, F)

Privilege posture for everything in this subsection: unprivileged process
with the `com.apple.security.hypervisor` entitlement — "The entitlement is
required to use the Hypervisor APIs in any process" [VERIFIED-DOC], macOS
11.0+. Self-signable by any Developer-ID app; no root, SIP untouched. This
is a real-product posture.

Confirmed surface, with what it carries:

| Facility | API | Since | Carries |
|---|---|---|---|
| Run/exit loop | `hv_vcpu_run`, exits CANCELED / EXCEPTION / VTIMER_ACTIVATED / UNKNOWN | 11.0 | execution substrate |
| Synchronous trap detail | `hv_vcpu_exit_exception_t { syndrome, virtual_address, physical_address }` (ESR-format syndrome, faulting IPA) | 11.0 | MMIO device exits, fault-driven dirty tracking |
| Debug exceptions to host | `hv_vcpu_set_trap_debug_exceptions` — "Sets whether debug exceptions exit the guest… equivalent system register is `MDCR_EL2.TDE`" | 11.0 | **P2 landing**: `BRK`, breakpoints, watchpoints, software step all exit to us |
| Guest debug state | `hv_sys_reg_t` includes `MDSCR_EL1`, `DBGBVR/DBGBCR0–15_EL1`, `DBGWVR/DBGWCR0–15_EL1`, `SPSR_EL1`, `ELR_EL1` | 11.0 | single-step (MDSCR.SS + SPSR.SS), run-to-PC |
| Timer authority | `hv_vcpu_set_vtimer_offset` ("corresponds to `CNTVOFF_EL2`"), `hv_vcpu_set_vtimer_mask` | 11.0 | epoch only — **not** a work clock; used solely as an invisible pacing kick |
| Guest-controlled counter trap | `HV_SYS_REG_CNTKCTL_EL1` accessible | 11.0 | **P1**: owned kernel clears `EL0VCTEN/EL0PCTEN` so guest-EL0 counter reads trap *to the guest kernel*, which serves V-time |
| Async kick | `hv_vcpus_exit` → CANCELED | 11.0 | stop-anywhere (invisible; never an injection point) |
| Interrupt injection | `hv_vcpu_set_pending_interrupt` (IRQ/FIQ) | 11.0 | **P2 delivery** at points we choose |
| Hypercalls | guest `HVC` arrives as EXCEPTION with EC `EC_AA64_HVC`; this is how every hvf VMM implements PSCI [CITED: QEMU `target/arm/hvf/hvf.c`] | 11.0 | **P2**: kernel doorbell vocabulary |
| WFI idle | WFx trap arrives as an exception exit; QEMU hvf sleeps the vcpu on it [CITED] | 11.0 | deterministic idle/skip points [MEASURE M-02 for exact EC + reliability] |
| Memory + protection | `hv_vm_map/unmap/protect` | 11.0 | snapshot/restore; software dirty log via write-protect faults |
| ID/feature regs | `hv_vcpu_config_get_feature_reg` | 11.0 | platform fingerprint recorded into snapshots |
| GICv3 device | `hv_gic_create` etc. | 15.0 | **deliberately unused** — we inject via IRQ pin + a paravirtual event controller in the owned kernel, removing Apple's GIC state machine from the deterministic surface |

Precedent that the debug path is real, not just documented: QEMU's hvf
backend implements full gdbstub guest debugging on Apple Silicon hosts —
hardware breakpoints, watchpoints, single-step — via exactly this surface
(patch series "Add gdbstub support to HVF", F. Cagnin, Nov 2022, merged
upstream in the QEMU 8.x line; guest and stub debug-register views are kept
separate) [CITED]. Step/breakpoint *exactness* under load is still ours to
prove: [MEASURE M-03].

Known holes in this surface (all [ESTABLISHED]/[VERIFIED-DOC], and all
worked around rather than wished away in §3B):

- No PMU is exposed to the guest and none of the `hv_sys_reg_t` values are
  PMU registers. For P1 this is a *feature* (the nondeterministic counter
  bank is unreachable by construction) [MEASURE M-04: confirm guest PMU
  reads undef/trap rather than returning host values].
- No run-until-N-events primitive; no dirty log; no counter-slope control.
- `HV_EXIT_REASON_VTIMER_ACTIVATED` and CANCELED are wall-clock-shaped and
  are therefore only ever used as invisible stops (stop, look, resume), never
  as injection points.

### 2.2 Virtual EL2 (macOS 15+, M3+)

`hv_vm_config_set_el2_enabled` / `hv_vm_config_get_el2_supported`, macOS
15.0+ [VERIFIED-DOC], M3-or-later hardware [CITED: Apple forums/release
notes]. Buys the monitor the architectural trap vocabulary of a real EL2
(HCR/MDCR/CNTHCTL semantics as emulated by Apple) — and, per E1, **no
PMU**. Each vEL1→vEL2 trap costs a hardware round trip through Apple's
EL2 plus re-injection into our monitor (NV-style), so the trap tax is a
multiple of plain-EL1 exits [MEASURE M-05 if ever pursued]. Fidelity of the
emulation is exactly the kind of surface Apple iterates on (E1 is itself an
emulation choice). Kept only as a possible future hardening layer (§3D).

### 2.3 Host performance-counter surfaces (the substrate for A)

- `thread_selfcounts` — private syscall, no SDK header, callable
  unprivileged (the spike used it; E3). Fixed counters only
  (instructions/cycles), no EL filter, self-thread only.
- kpc/kperf — private frameworks (`kperf.framework`,
  `kperfdata.framework` under `/System/Library/PrivateFrameworks`), kernel
  surface in XNU (`osfmk/kern/kpc.h`; arm64 backend accepts per-counter
  config words with EL-enable masks — `CFGWORD_EL0A64EN`/`EL1EN`/… in
  `osfmk/arm64/kpc.c` [CITED: opensource.apple.com xnu; re-verify against
  current release, M-06]). Event IDs come from the public-file/private-schema
  kpep databases in `/usr/share/kpep` (per-chip plists; `INST_BRANCH` 0x8d
  family, taken-branches 0x90 per E4). Configurable counting and
  `kpc_set_period` PMI require **root or the `com.apple.private.kernel.kpc`
  entitlement** [CITED: public users incl. FFmpeg
  `libavutil/macos_kperf.c`; recent M4-era usage reports]. SIP does not need
  to be disabled for the root path as far as public users report
  [MEASURE M-07 on the current OS].
- Topology note that makes EL filtering unusually clean here: on Apple
  Silicon the host XNU kernel runs at EL2 and host userspace at EL0, so
  **hardware EL1 is populated exclusively by guest kernels**. An EL1-only
  filtered counter on the vcpu thread counts guest-kernel work and nothing
  else. Guest EL0 shares EL0 with the VMM's own userspace, so an EL0+EL1
  work clock needs the VMM-side EL0 contribution between counter reads to be
  a deterministic constant [MEASURE M-08].

The PMI path exists for *sampling* (kperf actions), not for control: there
is no shipping path from a PMI to "stop this hv vcpu now," so even a working
PMI is a pacing hint delivered via a monitor thread + `hv_vcpus_exit`, with
milliseconds-class latency and unbounded overshoot in work terms. Under A,
exact positioning therefore still comes from stops + debug-step landing, not
from the PMI.

### 2.4 Structural absences (nothing to measure; they are simply not there)

No `perf_event_open` analogue; no public per-thread PMI with synchronous
delivery; no work-deadline `hv_vcpu_run`; no public dirty log; no guest PMU;
no CNTVCT slope control; Virtualization.framework (`VZVirtualMachine`) has
no vCPU register/exit surface at all and is a non-starter for any tier.

---

## 3. Alternatives

Performance tiers used below: **T0** ≤1.4× native-virt, **T1** 1.4–3×,
**T2** 3–50×, **T3** >10³×.

### A. Host-side kpc/kperf work clock, EL-filtered, with PMI (finish the blocked spike)

- **Mechanism.** The work clock is a host-side configurable PMC on the vcpu
  thread, event `INST_BRANCH`/0x8d family (or fixed instructions for the
  contamination experiment), EL-filtered via kpc config words so that only
  EL1 (guest kernel) — and, if M-08 holds, EL0+EL1 — is counted;
  per-thread accumulation via `kpc_set_thread_counting`; counter read at
  every stop. P2 arming: `kpc_set_period` PMI → kperf action → monitor
  thread → `hv_vcpus_exit` kick (pacing hint only), then authoritative
  exact landing by debug single-step walk to `work == target` (2.1).
  P1: unchanged from B's owned-kernel construction (kpc contributes nothing
  to P1).
- **Privilege posture.** Root (or Apple-private `com.apple.private.kernel.kpc`
  entitlement we cannot get). Shippable only as an admin-consented root
  helper; private API regardless. Lab/dev posture, not product.
- **Performance tier.** T0 on the record side (counting is passive); replay
  landing pays step-walks sized by PMI/kick skid [MEASURE M-09].
- **Determinism risks.** The M4 +73/74 contamination must *vanish* under an
  EL1-only filter — excluded, not modeled (kill condition if not). Host-EL0
  wrapper instructions inside the read window must be a per-exit-type
  constant for the EL0+EL1 variant (M-08). Counter attribution must stay
  exact across preemption/migration of the vcpu thread (kpc claims
  per-thread accumulation; M-10).
- **Fragility.** High. Private sysctl surface + per-chip kpep databases;
  Apple has changed this surface repeatedly and owes it no stability. Every
  macOS update requires revalidation; a hostile change bricks the tier.
- **Verdict.** Worth exactly one bounded spike (§5.2) — for the M4
  exclusion answer, as an independent oracle for B, and to keep an
  uninstrumented-guest lab tier alive. Never the product path.

### B. Guest software work clock: owned-toolchain instrumentation + doorbell quanta  ← primary

The inversion that dissolves the Apple problem: stop asking the host to
count guest work; make the guest carry its own work clock as architectural
state. Apple's API then only needs to do three things it verifiably does —
run the guest, hand us `BRK`/`HVC`/step exits at exact instruction
boundaries, and let us read/write guest registers at stops.

- **Mechanism sketch.**
  - *Unit.* "Work" = executed instrumentation sites. The toolchain (we own
    kernel, userspace, and compiler) reserves `x28` (`-ffixed-x28`) as the
    per-vCPU work counter and inserts `add x28, x28, #1` at every compiled
    basic block; NZCV-neutral, interrupt-atomic (single instruction), never
    spilled. V-time = f(x28) — the same pure-function contract as
    `docs/PARAVIRT-CLOCK.md`, new unit. 64-bit: no overflow in any horizon.
  - *Quantum / overshoot-impossible stop.* `x27` (`-ffixed-x27`) holds the
    host-owned threshold. At every loop backedge (and often enough in
    straight-line code that at most K sites separate consecutive checks;
    K is a pass-enforced constant), the pass emits
    `cmp x28, x27; b.lo 1f; brk #DOORBELL; 1:`. With
    `hv_vcpu_set_trap_debug_exceptions(true)`, the `BRK` exits to the host
    *before* anything else executes. Setting `x27 = W` stops the guest at
    the **first check site where x28 ≥ W** — a rule whose outcome is a pure
    function of guest state, hence identical on record and replay. Record
    normally parks `x27 = ∞` (zero check cost beyond the 2 instructions)
    and takes stops at kernel doorbells; when an event is scheduled at work
    W, both record and replay set `x27 = W` and get the *same site*. No
    hardware event, no PMI, no skid, no overshoot.
  - *Kernel doorbells.* The owned kernel issues `HVC` (SMCCC vendor range;
    PSCI-compatible conduit — hvf VMMs already receive `EC_AA64_HVC` exits
    [CITED]) at syscall boundaries, idle entry (with WFI exit as backstop),
    and its existing preemption points (`cond_resched` sites), passing x28
    naturally. These are the coarse event vocabulary; `BRK` check sites are
    the fine one.
  - *Injection (P2).* Only at stops that are pure functions of guest work:
    doorbell HVCs and threshold BRKs (plus synchronous traps). Delivery via
    `hv_vcpu_set_pending_interrupt` + a paravirtual event controller
    (shared-memory bitmap + IRQ pin) in the owned kernel — Apple's vGIC
    deliberately unused, so no third-party interrupt-controller state
    machine sits inside the deterministic boundary. CANCELED kicks and
    VTIMER activations are stop-look-resume only, never injection points
    [resume-transparency: M-11].
  - *Exact landing to arbitrary recorded points* (debug/branch workflows):
    set `x27 = W − K`, run to the guaranteed-undershoot BRK, then
    single-step (MDSCR_EL1.SS + SPSR.SS via `hv_sys_reg_t`, step exits per
    2.1) to (x28 == W, PC == target). Within one x28 value at most one
    dynamic instance of any PC exists (every BB is a site), so (W, PC) names
    a unique instruction instance. Cost: ≤ K·avg-BB steps, milliseconds at
    K ≈ 64. This is C's machinery demoted to a subroutine.
  - *P1 inventory* (each named source, its carrier):
    - CNTVCT/CNTPCT from guest EL0: trapped **inside the guest** by owned
      kernel via `CNTKCTL_EL1.EL0VCTEN/EL0PCTEN = 0` (register verifiably
      guest-accessible, 2.1); handler returns V-time.
    - CNTVCT from guest EL1: made unreachable — owned kernel has no
      arch-timer clocksource; a link-time audit forbids the `MRS …,
      CNTVCT_EL0/CNTVCTSS/CNTPCT…` encodings in kernel and userspace images
      (owned toolchain ⇒ enforceable in CI).
    - vDSO/`gettimeofday`: owned kernel's vDSO serves V-time only.
    - Entropy: no FEAT_RNG expected on M-silicon (RNDR absent)
      [MEASURE M-12 via `ID_AA64ISAR0` under hvf]; virtio-rng from the
      seed-derived monitor stream regardless; audit forbids RNDR encodings.
    - PMU from guest: absent under hvf (2.1, M-04) — unreachable.
    - Identity: `MIDR/MPIDR` pinned via sys-reg writes where settable
      (MPIDR is, per Apple's own GIC note [VERIFIED-DOC]; MIDR: M-13),
      otherwise recorded into the snapshot's platform fingerprint via
      `hv_vcpu_config_get_feature_reg`; ID-reg reads happen once in early
      boot of the owned kernel.
    - WFE/event-stream pacing: invisible by construction — wake pacing
      changes when iterations happen in wall time, never how many happen
      before a state change, because every state change is injected at a
      work-count-defined point; owned kernel additionally avoids the event
      stream.
    - PAC: disabled in the owned kernel (`SCTLR_EL1.EnIA…=0`) to keep
      IMPDEF QARMA out of guest-visible state (or pinned keys if wanted
      later).
  - *Count integrity rules* (the "what invalidates counts" answer): x27/x28
    are global-fixed registers, never saved/restored per task, skipped in
    signal-frame and `switch_to` restore paths (owned-kernel patch list);
    hand-written assembly is uninstrumented by design — bounded,
    deterministic dead zones (counts don't advance across `memcpy`; (W, PC)
    still unique because the surrounding sites are). Counts are an artifact
    of the exact guest image: record/replay/branch bind to the image hash —
    already true of every backend (PMU counts are binary-tied too). Guest
    JIT is out of scope initially; when wanted, the JIT is ours and emits
    sites like the compiler.
  - *Snapshot/restore/branch.* Full register file (x27/x28 included — the
    clock snapshots itself), `hv_sys_reg_t` state, memory (software dirty
    log via `hv_vm_protect` write-protect faults, 2.1), monitor device
    state. State-completeness harness required [MEASURE M-14].
  - *SMP.* Unchanged policy from existing backends: one runnable vCPU at a
    time, quantum-interleaved by work count; per-vCPU x27/x28 come free.
- **Privilege posture.** `com.apple.security.hypervisor` entitlement only.
  No root, no SIP change, no private API. Product-shippable, App-Store-class
  posture.
- **Performance tier.** T0 estimated: +1 ALU op per BB (~+10–20% dynamic
  instructions, less in time), +2 ops per backedge, exits only at quanta
  and device I/O. Envelope 1.1–1.4× over plain hvf [MEASURE M-01, with the
  hvf exit round-trip cost measured as M-15].
- **Determinism risks.** (i) hvf itself leaking nondeterminism into
  guest-visible state (in-kernel emulated sysregs, exception-syndrome
  variability): gated by the dual-run + sysreg-sweep experiment — this is
  the kill condition, §5.1; (ii) debug-exception delivery reliability
  (missed/duplicated BRK or step exits): M-03; (iii) count-integrity bugs in
  the kernel patch list: caught structurally by the differential harness
  (any lost increment shows as a divergence at the next stop); (iv) M4-class
  host-kernel guest re-entries (the +73/74 phenomenon): *harmless here even
  if present* — they perturb a host-side observer, not guest architectural
  state; x28 is immune by construction. This asymmetry is the core argument
  for B over A.
- **Fragility.** Low. Every API used is public, documented, and stable
  since macOS 11 (only the entitlement and `hv_vcpu_run` semantics are
  load-bearing; vGIC and vEL2 are unused). OS updates get a cheap automated
  re-run of the determinism gate. Residual risk: Apple altering debug-trap
  or HVC exit behavior — architectural surfaces with public commitments,
  the safest bet available on this platform.
- **Verdict.** Primary spike. The only design on the list that carries both
  properties with public API at T0.

### C. Debug-architecture-only execution (no work counter at all)

- **Mechanism.** Position is tracked purely by the debug architecture:
  single-step everything (step exit per instruction), or breakpoint-bounded
  runs with occurrence counting (place DBGBVR breakpoint on the target PC,
  count hits). P1 as in B (it is guest-construction, not counting). P2:
  every instruction boundary is a stop, so injection anywhere.
- **Privilege posture.** Same as B (public API).
- **Performance tier.** T3. One hvf exit per instruction; at the 1–5 µs
  class round trip [M-15] this is ~0.2–1 M guest instructions/sec,
  10³–10⁴× slowdown. Breakpoint-count replay is unbounded in the worst case
  (a hot PC revisited 10⁹ times before its target occurrence = 10⁹ exits).
  Determinism itself survives interrupts/WFI (injection is at step
  boundaries; WFI is just another stop), it is only the cost that is
  prohibitive.
- **Verdict.** Not a tier to build for its own sake. It survives inside B
  as the exact-landing subroutine, and as the forensic last resort
  (replaying a suspect window at step granularity to localize a divergence).

### D. Virtual-EL2 monitor without a nested PMU

- **Mechanism.** Our monitor at vEL2 (macOS 15+, M3+ [VERIFIED-DOC]),
  Linux at vEL1/EL0; monitor-owned software counting at whatever it traps
  (MDCR/CNTHCTL/HCR vocabulary as Apple emulates it). Without a PMU the
  monitor sees work only at trap boundaries — syscall-class granularity at
  best, nowhere near a work clock. To become viable it must import B's
  instrumented guest — at which point vEL2 adds only defense-in-depth traps
  (e.g. CNTVCT trapping if Apple's emulation implements FEAT_ECV-style
  controls: unknown, M-16) at a multiplied exit cost (M-05) on the youngest,
  most-iterated emulation surface Apple ships (E1 is the proof).
- **Privilege/perf/fragility.** Same entitlement as B; slower than B;
  fragility markedly higher (nested-virt emulation fidelity).
- **Verdict.** Deferred. Re-open only if a future macOS ships a nested PMU
  (§6) or an audit demands trap-enforced (not construction-enforced) P1.

### E. Deterministic binary translation (QEMU TCG icount class)

- **Mechanism.** Full software CPU: QEMU TCG with `-icount`
  (instruction-counted virtual time, deterministic execution; upstream
  record/replay exists) [CITED: QEMU docs]. Both properties carried inside
  one process we fully control; macOS is reduced to a POSIX host; Apple
  surface: none.
- **Privilege posture.** None. Not even the hypervisor entitlement.
- **Performance tier.** T2 (~10–50×).
- **Determinism risks.** Well-trodden; device models must be ours (same PV
  set as B). Fidelity: TCG's architectural model, not Apple's CPU — fine
  for a same-image oracle, imperfect as a bug-compatible twin.
- **Fragility.** None w.r.t. Apple.
- **Verdict.** Adopt (not spike) as the standing slow oracle: every B
  payload also runs under TCG icount in CI; any three-way disagreement
  (B-run-1, B-run-2, TCG) localizes fault. Also the guaranteed fallback
  tier if B is killed: Consonance-on-macOS would exist at T2.

### F. Replay-only macOS tier

- **Mechanism.** Record on a supported Linux backend; replay/debug/branch
  on macOS. What replay alone requires from macOS: deterministic
  re-execution + exact landing at recorded points — i.e. exactly B's
  substrate; a hardware clock is *not* on the list. The clean unification:
  build guest images with B's instrumentation everywhere (increments are
  branch-free straight-line code, so they do not perturb the hardware
  branch-event counts the Linux backends use); Linux records log, at every
  event, both the hardware work count *and* the guest software clock
  (x28 — readable guest state at any stop). macOS replays land on the
  logged x28 values via B's landing rule. No cross-unit conversion, ever —
  both clocks are logged, each side lands on its own.
- **Privilege/perf/fragility.** As B.
- **Verdict.** Not a separate design — it is B's first shipped milestone
  (replay is strictly easier than record: the event schedule is given).
  Also the honest fallback if B's *record* side dies on an hvf
  nondeterminism that replay can tolerate structurally (replay pins every
  injection; record must also merely observe).

### G. Adjacent, and honestly not macOS: Linux/KVM bare metal on Apple Silicon (Asahi lineage)

- **Mechanism.** The existing ARM backend (per `docs/ARM-PORT.md` /
  `docs/ARM-ALTRA.md`) on M-series silicon under Linux: KVM + real PMU
  (apple event family per E4, exactness precedented by rr on M1/M2).
- **Status.** M1/M2: supported platform class. M3: boots as of early 2026,
  early-stage (no GPU, unshippable, no ETA); M4/M5: early bring-up [CITED:
  Asahi progress reports / Phoronix, Jan–Feb 2026]. KVM+PMU status on
  M3+: unverified [M-17].
- **What it validates.** That M-series *silicon* meets Consonance's
  counting/stepping standards, and it gives M-hardware owners a
  Consonance path via dual-boot. **What it does not validate:** anything
  about macOS — no Hypervisor.framework, no kpc, no Apple software surface
  at all. It answers the brief's question for the hardware while
  explicitly leaving its OS.
- **Verdict.** Keep as a named adjacency. If B's spike succeeds, G is
  deprioritized; if Apple's surfaces ever regress, G is the M-hardware
  escape hatch (M1/M2 only today).

### H. Non-starters (named to close the space)

- **Virtualization.framework:** no vCPU register access, no exit loop, no
  debug traps — carries neither property at any tier.
- **Unmodified-hvf with wall-clock event injection:** violates P2 by
  definition (E2's vocabulary is wall-clock-shaped); listed only because it
  is the default VMM shape everyone else builds.
- **Private-framework GPU/ANE offload tricks, third-party kexts, SIP-off
  research configs:** fail the "shipping Apple software / viable posture"
  constraint and are not pursued.

---

## 4. Ranking

1. **B** — guest software work clock. Only candidate carrying P1+P2 at T0
   on public API with a product-viable posture. Bounded, cheap, falsifiable
   day-one experiment; its failure mode (hvf guest-visible nondeterminism)
   would also kill A/C/D/F, so it is the right first question to put to the
   hardware.
2. **A** — kpc spike completion, root lab box, timeboxed. Settles E3's open
   question (EL-filter exclusion of the M4 contamination), and gives B an
   independent uninstrumented oracle. Product path: no.
3. **E** — standing TCG icount oracle; adopt, no spike.
4. **F** — folds into B (first milestone: replay).
5. **C** — subroutine of B + forensic tier; no standalone investment.
6. **D** — deferred pending a nested PMU or a trap-enforcement requirement.
7. **G** — adjacency, tracked, not funded.

Recommended spike order: **B first** (needs no root, no special box), **A in
parallel** the moment a root-capable M4 lab box exists (it was blocked on
exactly that).

## 5. Spike specifications

### 5.1 Spike B — day one

**Experiment** (one C file + ~300 lines of payload asm; no Linux, no
toolchain pass yet — increments/checks hand-emitted):

1. hvf VM, one vCPU, flat RAM, debug traps on. EL1 payload: nested loops
   with seed-parameterized iteration matrix; `add x28,…` at every BB,
   threshold check + `brk` at backedges (K enforced by hand), `hvc` at
   outer-loop doorbells. Analytical oracle gives exact expected x28 at
   every doorbell/BRK stop.
2. Matrix: quantum ∈ {10³, 10⁵, 10⁷} sites × host ∈ {idle, fully loaded}
   × ≥100 full runs, totalling ≥10⁶ stops. At every stop assert:
   x28 == oracle, PC == expected site, and a full GPR/sysreg/memory hash
   identical across runs at corresponding stops.
3. Mid-run at a random stop: snapshot (full state dump), restore into a
   fresh VM, continue — hashes must match the uninterrupted run bit-for-bit.
4. Injection leg: at stop #k, pend IRQ; vector stub logs x28 at entry;
   delivery point must be identical across all runs.
5. Week-one extensions (same harness): sysreg sweep at EL1 (read the
   encodable space, diff across runs and across snapshot/restore); step-walk
   10⁴ instructions comparing trajectories (M-03); CANCELED-kick
   resume-transparency storm (M-11); hvf exit RTT measurement (M-15).

**Acceptance floor:** zero mismatches — counts, PCs, state hashes — over
≥10⁶ stops across ≥100 runs including loaded-host runs; zero divergence
after snapshot/restore; injection point bit-identical in 100/100 runs.

**Kill condition:** any guest-visible divergence between identically-seeded
runs attributable to hvf itself (not a harness bug) that cannot be made
unreachable by owned-guest construction — e.g. a sysreg the guest cannot
avoid whose value varies, nondeterministic syndrome/priority on the exits we
depend on, or unreliable BRK/step delivery (missed or duplicated debug
exceptions). Secondary kill: CANCELED stop/resume perturbs guest state
(breaks pacing and snapshots, hence record).

### 5.2 Spike A — day one (root M4 box, macOS 26.x)

**Experiment:**

1. Root process: acquire counters (`kpc_force_all_ctrs`-class), program a
   configurable counter with the INST_BRANCH-family event and an EL1-only
   config word; enable per-thread accumulation on the vcpu thread; run the
   *same payload and oracle as 5.1* under hvf; read at every stop.
2. Re-run E3's exact contamination matrix (the window sizes that produced
   +73/74 on M4) under (a) fixed-instruction counters unfiltered
   (reproduce), (b) EL1-only filter (must vanish), (c) EL0+EL1 filter
   (characterize the host-EL0 wrapper constant, M-08).
3. PMI leg: `kpc_set_period` on the counter, kperf action; measure
   delivery latency and overshoot distribution against the oracle
   (M-09); wire action → monitor thread → `hv_vcpus_exit` and measure
   stop-skid in work units.

**Acceptance floor:** EL1-only filtered counts equal the analytical oracle
with zero mismatch over ≥10⁶ windows *including* the previously
contaminated M4 window sizes (the +73/74 must be absent, not smaller);
per-thread attribution stays exact under forced preemption/migration
(M-10); PMI demonstrably fires and its latency/overshoot distribution is
characterized (any finite bound is a pass for a *hint*; there is no exactness
requirement on PMI).

**Kill condition:** contamination persists inside EL1-only counts; or kpc
rejects branch-family events/EL filtering on M4; or filtered counts drift
by even 1 over the trial mass; or macOS 26.x denies root kpc without a
private entitlement. Any of these closes A permanently (the product path
never depended on it).

### 5.3 Standing oracle (no spike)

Bring up the 5.1 payload under `qemu-system-aarch64 -accel tcg -icount
shift=…,sleep=off`: assert the same architectural trajectory. Wire into CI
as the third opinion for every future divergence triage.

## 6. What Apple would have to ship (for what stays blocked)

**Exists but gated** (the software exists in shipping macOS; the gate is
policy):
- kpc/kperf configurable counting, EL filtering, `kpc_set_period` PMI —
  gated behind root / `com.apple.private.kernel.kpc`. A public counted-thread
  API (or just blessing the sysctl surface) unblocks A as a supported tier.
- kpep event databases — public files, private schema, no stability
  promise.
- `thread_selfcounts` — callable today but headerless and fixed-counter
  only; a supported header + EL filter would have made E3's spike
  conclusive without root.

**Exists in silicon, absent in Apple's software:**
- Guest-visible PMU under virtualization. The cores count branches
  precisely (E4); plain-EL1 guests get no PMU at all (2.1), and the vEL2
  emulation advertises `PMCR_EL0.N = 0` (E1). Apple virtualizing the PMU —
  even one counter + overflow interrupt — reopens the original
  hardware-work-clock design (and D).
- FEAT_ECV-style counter trapping for guests (whether M-silicon has ECV at
  all: M-16).

**Does not exist (would have to be designed, not just ungated):**
- A work-deadline run primitive (`hv_vcpu_run` until counter == target) —
  no analogue anywhere in shipping XNU/hvf.
- A public per-thread PMI with synchronous, low-skid delivery
  (`perf_event_open`-class semantics).
- A dirty-page log for hvf (worked around via `hv_vm_protect`
  write-protect faults; a real log is an efficiency gift, not a blocker).
- CNTVCT slope control (offset only today, `CNTVOFF_EL2` semantics
  [VERIFIED-DOC]).

None of these are on B's critical path; that is the point of B.

## 7. Measurement ledger (everything on-hardware, numbered)

| # | Question | Blocking? |
|---|---|---|
| M-01 | B end-to-end overhead vs plain hvf (instrumentation + quantum exits) | tier claim only |
| M-02 | WFI exit EC + reliability under hvf | B idle path |
| M-03 | Debug-exception exactness: no missed/duplicated BRK or step exits under load | B kill-relevant |
| M-04 | Guest PMU reads under hvf: undef/trap vs emulated values | P1 audit |
| M-05 | vEL2 nested trap round-trip cost | D only |
| M-06 | Current-xnu kpc EL-config-word surface unchanged | A |
| M-07 | Root kpc with SIP enabled on macOS 26.x | A |
| M-08 | Host-EL0 wrapper contribution constant per exit type | A (EL0+EL1 clock) |
| M-09 | kpc PMI delivery latency + overshoot distribution | A |
| M-10 | kpc per-thread attribution exactness across preemption/migration | A |
| M-11 | CANCELED stop/resume transparency (state hash unchanged) | B kill-relevant |
| M-12 | `ID_AA64ISAR0.RNDR` under hvf (expect absent) | P1 audit |
| M-13 | MIDR_EL1 settable via `hv_vcpu_set_sys_reg`? | snapshot portability nicety |
| M-14 | Snapshot state completeness (get/set round-trip covers all guest-relevant state) | B milestone 2 |
| M-15 | hvf exit round-trip cost (BRK, HVC, MMIO, step) | tier math |
| M-16 | FEAT_ECV presence (silicon + vEL2 emulation) | D/P1 hardening only |
| M-17 | KVM + PMU status under Asahi on M3/M4 | G only |

## 8. Citations

Apple documentation (fetched 2026-08-06 via developer.apple.com
documentation JSON; availability as stated):

- `hv_vm_config_set_el2_enabled` — macOS 15.0+. `hv_vm_config_get_el2_supported` for platform support (M3+ per Apple developer forum guidance and vendor release notes).
- `hv_vcpu_set_trap_debug_exceptions` — macOS 11.0+; "Sets whether debug exceptions exit the guest."; "The equivalent system register is `MDCR_EL2.TDE`."
- `hv_exit_reason_t` — `HV_EXIT_REASON_CANCELED` ("exits requested by exit handler on the host"), `HV_EXIT_REASON_EXCEPTION` ("traps caused by the guest operations"), `HV_EXIT_REASON_VTIMER_ACTIVATED`, `HV_EXIT_REASON_UNKNOWN`.
- `hv_vcpu_exit_exception_t` — fields `syndrome`, `virtual_address`, `physical_address` (IPA).
- `hv_vcpu_set_vtimer_offset` — macOS 11.0+; "This corresponds to the value of the `CNTVOFF_EL2` register."
- `hv_vcpu_set_vtimer_mask`, `hv_vcpu_set_pending_interrupt`, `hv_vm_protect`, `hv_vm_map` — macOS 11.0+.
- `hv_sys_reg_t` — includes `HV_SYS_REG_MDSCR_EL1`, `HV_SYS_REG_DBGBVR0–15_EL1`/`DBGBCR`/`DBGWVR`/`DBGWCR`, `HV_SYS_REG_CNTKCTL_EL1`, `HV_SYS_REG_SPSR_EL1`, `HV_SYS_REG_ELR_EL1`; no PMU registers listed.
- `hv_vcpu_config_get_feature_reg` — macOS 11.0+.
- `hv_gic_create` — macOS 15.0+; GICv3 device, one per VM; "vCPUs must set affinity values in their `MPIDR_EL1` register."
- Entitlement `com.apple.security.hypervisor` — macOS 11.0+; "The entitlement is required to use the Hypervisor APIs in any process."

Third-party shipping code (mechanism precedents):

- QEMU `target/arm/hvf/hvf.c` — Apple Silicon hvf support (A. Graf, 2020–21): guest `HVC` arrives as an exception exit (`EC_AA64_HVC`) and is the PSCI conduit; WFx trap handling; sysreg save/restore over `hv_vcpu_*_sys_reg`.
- QEMU hvf gdbstub series — "Add gdbstub support to HVF" (F. Cagnin, Quarkslab, Nov 2022; merged in the 8.x line): guest debugging on Apple Silicon hosts via `hv_vcpu_set_trap_debug_exceptions`, hardware breakpoints/watchpoints, single-step; guest vs stub debug-register views split (P. Maydell review).
- QEMU icount / record-replay documentation — deterministic instruction-counted execution (basis for E).
- FFmpeg `libavutil/macos_kperf.c` — public consumer of the private kpc API (root-gated `kpc_set_config`/`kpc_set_thread_counting` usage pattern).
- XNU (opensource.apple.com) — `osfmk/kern/kpc.h` and the arm64 kpc backend with per-counter EL-enable config words (`CFGWORD_EL0A64EN`/`EL1EN`/…); re-verify against the current release (M-06).
- rr on Apple Silicon under Linux — Apple taken-branch event 0x90 on M1/M2 (per E4, established by the prior program).
- Asahi Linux progress reports (Jan–Feb 2026) + Phoronix/AppleInsider coverage — M3 boots, early-stage, no ETA; M4/M5 early bring-up (basis for G's status).

Consonance repo cross-references (not available in this session; cited as
targets): `docs/APPLE-SILICON.md` (dead program + evidence standards),
`docs/ARM-PORT.md`, `docs/ARM-ALTRA.md`, `docs/PARAVIRT-CLOCK.md` (V-time =
f(work) contract that B preserves), `docs/GLOSSARY.md`, beads `hm-ssz`,
`hm-dj0`.
