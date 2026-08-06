# Consonance on macOS / Apple Silicon (M3+): Alternatives Design Memo

Status: **v2**, for review. v1 written 2026-08-06 from the task brief plus
Apple-documentation verification; v2 same day, after cross-review against an
independent second memo (GPT-authored) that had two sources this session did
not: the Consonance repo itself (including the retained
`spike/as2h-host-count` branch at `500f95d` and bead `hm-ssz`) and the local
macOS 26.4.1 SDK headers. §0.1 lists what changed and why; §9 records the
full adjudication — what was accepted, what was rejected, and the reasons.

Evidence labels: [VERIFIED-DOC] checked against Apple's online documentation
on 2026-08-06 (§8); [SDK] cited by the cross-review memo from the local
macOS 26.4.1 SDK headers, re-verify in CI against our own SDK; [REPO-EVID]
measured results retained in the Consonance repo (spike branch / beads);
[XNU] cited to apple-oss-distributions/xnu at commit `f6217f89…`, re-verify
against the shipping release; [CITED] third-party shipping source; [MEASURE
M-nn] on-hardware only, ledger in §7. Where a design names a counter, event,
or mechanism, that exact one is meant; no silent substitution.

---

## 0. Verdict

There is **no credible hardware work-clock path** on shipping macOS for
M3-or-later — this is now supported by measurement, not just absence of API.
There **are** two credible software paths that preserve the bit-identical
contract, and the two memos converged on them independently:

1. **Primary spike — Alternative B, guest software work clock (`SW_EDGE_v1`)
   on direct-EL1 Hypervisor.framework.** The work clock is architectural
   guest state maintained by our owned toolchain in every executable guest
   component; deadlines fire as guest-initiated `HVC` (EL1) / `SVC→HVC`
   (EL0) exits — exact, overshoot-impossible, wall-clock-free. The debug
   architecture is *not* on the correctness path (changed from v1). Public
   API only; `com.apple.security.hypervisor` entitlement; no root, no SIP
   change. Estimated 1.2–5× [MEASURE M-01]; correctness never traded for
   the estimate — miss the envelope and the tier demotes, the contract
   stays.

2. **Parallel fallback — Alternative E1, deterministic x86-64 TCG with an
   exact translated `BR_INST_RETIRED.CONDITIONAL` clock.** Reuses the
   existing x86-64 guest, machine contract, and film format; needs no Apple
   virtualization surface at all; ~10–100×. Its slow path can start in
   parallel because it is independent of everything Apple.

Closed or demoted:

- **Host kpc/kperf (A): strong NO-GO as an architecture.** Retained M4
  measurements show the tested configurable counters are **guest-blind**
  (constant 74/312 regardless of guest work) while the fixed counters carry
  the guest slope but with **unfilterable** kernel contamination — current
  XNU hardwires fixed counters to count all modes and exposes no privilege
  filter for them [REPO-EVID][XNU]. One corrected RAWPMU/force-all probe
  (the prior probe had an ordering flaw) is worth **half a day** on an
  already-rooted M4, no more. v1 ranked this as spike #2; that is
  withdrawn.
- **Debug-architecture-only (C):** oracle / slow replay engine, gated on
  one hardware question (exact one-retired-instruction stepping); also the
  landing subroutine inside B's debug workflows. Not a product path.
- **vEL2 without a PMU (D):** closed by construction — trap ordinal is not
  a work clock; every viable variant reduces to B or C.
- **Replay-only (F):** not a fourth mechanism; replay still needs an
  authoritative position clock, so it folds into B (shared `SW_EDGE_v1`),
  E (TCG), or C.
- **Linux bare metal on M-series (G):** adjacent, leaves macOS, and not
  ready on M3/M4 (PMU support marked TBA in Asahi's feature tables
  [CITED]).

### 0.1 What changed v1 → v2

1. **A is dead as a spike.** v1's EL-filter hypothesis assumed (i) the
   configurable counter bank sees guest execution and (ii) fixed counters
   are kpc-filterable. Repo evidence refutes (i) on M4; XNU's
   `monotonic_arm64.c` refutes (ii) (fixed counters hardwired to all modes,
   owned by monotonic, not kpc config) [REPO-EVID][XNU]. The two xnu
   readings reconcile: the `CFGWORD_EL*EN` filter words v1 cited do exist
   in the kpc configurable path — they are moot if that bank never counts
   the guest. Replaced by a half-day corrected closure probe (§5.3).
2. **B's correctness carrier moved off the debug channel.** v1 fired
   deadlines via `BRK` + `hv_vcpu_set_trap_debug_exceptions`. v2 fires them
   as branches to an `HVC` thunk at EL1 and `SVC` into a patched IRQ-masked
   EL1 trampoline at EL0. Only architecturally guaranteed synchronous traps
   remain on the kill path; debug exceptions are demoted to debug/landing
   workflows and an optional measured optimization (M-03 is no longer
   kill-relevant for record/replay).
3. **New closure obligations adopted into B:** LL/SC excluded (host exits
   and debug operations can clear the exclusive monitor and change retry
   paths — a determinism leak v1 missed); LSE-only builds enforced by
   audit. Pending IRQ state is cleared after each `hv_vcpu_run` [SDK], so
   injection uses a software latch reasserted every entry until acceptance.
   WFI/WFE are compiled out in favor of deterministic idle hypercalls
   (v1's WFE-invisibility analysis is kept as defense-in-depth, not load-
   bearing). Negative tests (fail-closed on uninstrumented edges, forbidden
   encodings, W^X violations, raw WFI) added to the acceptance floor.
4. **Clock-state variant question opened honestly:** v1's reserved-register
   monotonic counter vs the cross-review's memory-backed countdown. Each
   has a named hazard (restore-path clobber vs non-atomic-RMW erasure).
   Day-one payload A/Bs both (§3B, §9-R2).
5. **E split into E1/E2.** E1: x86-64 TCG as the product fallback with an
   exact `BR_INST_RETIRED.CONDITIONAL` translated clock — film-compatible
   with the Intel backend, which v1's arm64-oracle framing missed. E2:
   arm64 TCG icount retained as the cheap differential oracle for B
   payloads. Stock icount and stock QEMU record/replay are explicitly *not*
   the clock/contract (§3E).
6. **Perf envelope for B widened** from v1's 1.1–1.4× to 1.2–5× pending
   M-01, with the demote-don't-relax rule adopted.
7. Inventory additions [SDK]: per-run-cleared pending interrupts; opaque
   GIC state save/restore that may legitimately fail to restore across OS
   versions (reinforces v1's decision to avoid the vGIC); a PMU-access
   trap configuration surface in the macOS 26 SDK's `hv_vm_config.h`
   (upgrades v1's M-04 from "hope it traps" to "configure it and verify").
8. Terminology aligned to the repo: recordings are **films**; the new unit
   is named and versioned (`SW_EDGE_v1`) and is never described as, or
   converted to, hardware `BR_RETIRED`. Films made with hardware clocks
   and films made with `SW_EDGE_v1` are distinct artifacts; where both
   clocks are carried, both are logged, and each side lands on its own.

---

## 1. Ground rules

The two properties any design must carry, verbatim from the program:

- **P1 (no nondeterministic results):** every guest-visible nondeterministic
  result (time, entropy, identity, counters) is trapped or made unreachable
  and replaced by a deterministic value.
- **P2 (exact async injection):** every asynchronous event is injected at an
  exact, reproducible point in guest work — never at wall-clock time.

Binding constraints: bit-identical is never relaxed; cooperative guest
(owned kernel, userspace, toolchain) allowed; shipping Apple software only,
privilege posture stated per design; "unsupported" is a result.

Established facts carried forward:

- E1. vEL2 on M4/macOS 26.5 advertises `PMCR_EL0.N = 0`, no `PMICNTR`
  [ESTABLISHED, `docs/APPLE-SILICON.md`].
- E2. Plain-EL1 exit vocabulary is CANCELED / EXCEPTION / VTIMER_ACTIVATED
  / UNKNOWN [VERIFIED-DOC][SDK `hv_vcpu_types.h`]; `hv_vcpus_exit` is an
  immediate kick, not a deadline [SDK `hv_vcpu.h`]; `CNTVCT_EL0` =
  `mach_absolute_time() − offset` — epoch movable, slope not
  [SDK][VERIFIED-DOC `hv_vcpu_set_vtimer_offset` ≙ `CNTVOFF_EL2`].
- E3 (updated). Host-side counting on M4/macOS 26.5, from the retained
  spike branch [REPO-EVID]: configurable `INST_BRANCH` constant at 74 and
  `INST_ALL` at 312 across varying guest loop sizes (guest-blind);
  fixed instruction counter guest-sloped but with state-dependent +8,
  +73/74, +82 and rare async host/kernel contributions (guest-inclusive,
  contaminated, unfilterable). `thread_selfcounts` is a private read-only
  SPI (`bsd/sys/resource_private.h` [XNU]) usable without root but
  unfilterable — a heuristic, never a position authority.
- E4. The silicon counts precisely (rr on M1/M2 under Linux, Apple
  taken-branch event 0x90). The gap is Apple's software surface. Event IDs
  are microarchitecture/database-specific: never carry an event number
  across M-generations without resolving it from that machine's kpep
  database and logging the database hash.

---

## 2. The macOS mechanism inventory

### 2.1 Hypervisor.framework, plain-EL1 guest (substrate for B, C, F)

Privilege posture for this whole subsection: ordinary signed process with
`com.apple.security.hypervisor` — "required to use the Hypervisor APIs in
any process" [VERIFIED-DOC], macOS 11.0+. No root, no SIP change. Real
product posture.

| Facility | Surface | Carries | Notes |
|---|---|---|---|
| Run/exit loop | `hv_vcpu_run`; exits CANCELED / EXCEPTION / VTIMER_ACTIVATED / UNKNOWN [VERIFIED-DOC][SDK] | execution substrate | no PMU/work exit exists |
| Synchronous traps | `hv_vcpu_exit_exception_t { syndrome, virtual_address, physical_address }` [VERIFIED-DOC] | **P2 carrier**: `HVC`/`SVC`-driven doorbells; MMIO exits; fault-driven dirty tracking | guest `HVC` arrives as EXCEPTION with `EC_AA64_HVC` — the PSCI conduit every hvf VMM uses [CITED: QEMU `target/arm/hvf/hvf.c`]; exact syndrome path day-one confirm [M-18] |
| Debug traps | `hv_vcpu_set_trap_debug_exceptions` ("debug exceptions exit the guest… equivalent is `MDCR_EL2.TDE`") [VERIFIED-DOC]; `MDSCR_EL1`, `DBGB/W{V,C}R0–15_EL1`, `SPSR_EL1`, `ELR_EL1` in `hv_sys_reg_t` [VERIFIED-DOC] | debug/landing workflows only (v2) | Apple documents **no** "exactly one retired instruction" contract [SDK]; gate M-03 before any authoritative use |
| Interrupts | `hv_vcpu_set_pending_interrupt` (IRQ/FIQ) [VERIFIED-DOC] | **P2 delivery** | pending state cleared after each `hv_vcpu_run` [SDK `hv_vcpu.h`]: VMM keeps a deterministic software latch, reasserts every entry until guest acceptance |
| Timer | `hv_vcpu_set_vtimer_offset` / `_mask` [VERIFIED-DOC] | nothing deterministic | epoch only; VTIMER_ACTIVATED used, if at all, as an invisible pacing stop |
| Guest counter control | `HV_SYS_REG_CNTKCTL_EL1` accessible [VERIFIED-DOC] | **P1**: owned kernel traps guest-EL0 counter reads to itself, serves V-time | |
| PMU access | no PMU regs in `hv_sys_reg_t` [VERIFIED-DOC]; macOS 26 SDK `hv_vm_config.h` exposes PMU-access trap configuration [SDK] | **P1**: PMU unreachable, now configurable rather than assumed | verify trap-vs-undef behavior [M-04] |
| Memory | `hv_vm_map/unmap/protect` [VERIFIED-DOC] | snapshot/restore; software dirty log via write-protect faults | no public dirty log — speed gap, not correctness [M-19 for `hv_vm_protect` fault behavior] |
| Kick | `hv_vcpus_exit` → CANCELED [VERIFIED-DOC][SDK] | stop-look-resume only | must be semantically invisible [M-11]; **never** an injection point |
| GIC | `hv_gic_create` (GICv3, macOS 15+) [VERIFIED-DOC]; opaque state save/restore may legitimately fail across software versions [SDK `hv_gic_state.h`] | — | deliberately unused: PV event controller + IRQ-pin latch instead; if ever used, fail closed on restore incompatibility |
| ID/feature regs | `hv_vcpu_config_get_feature_reg` [VERIFIED-DOC] | platform fingerprint in snapshots | |

### 2.2 Virtual EL2 (macOS 15+, M3+)

`hv_vm_config_set_el2_enabled` [VERIFIED-DOC, macOS 15.0+; M3+ hardware].
No PMU behind it (E1). Each vEL1→vEL2 trap multiplies exit cost through
Apple's NV-style emulation [M-05]. See §3D: without a work source of its
own it cannot be a clock; with one it is redundant. Closed except as a
possible future single-step accelerator (§3C) or if Apple ships a nested
PMU (§6).

### 2.3 Host performance counters — now measured, and negative

What v1 treated as open hypotheses, the retained spike resolves [REPO-EVID],
and XNU source explains [XNU]:

- **Configurable kpc counters are guest-blind on the tested M4**: constants
  (74 / 312) independent of guest work. The EL-filter config words exist in
  the kpc configurable path, but a bank that never counts guest execution
  has nothing to filter.
- **Fixed counters are guest-inclusive but unfilterable**: owned by the
  monotonic subsystem, hardwired to count all modes
  (`osfmk/arm64/monotonic_arm64.c`), with no privilege filter; RAWPMU's
  exposed register list does not include fixed-counter `PMCR1`
  (`osfmk/arm64/kpc.c`). The observed +8/+73/74/+82 contamination therefore
  **cannot be excluded by any current configuration**, root or not.
- **Privilege**: kpc ownership is root or a root-blessed PID
  (`bsd/kern/kern_kpc.c`, `osfmk/kperf/kperfbsd.c`, `bsd/kern/kern_ktrace.c`);
  the private-entitlement bypass is compiled for development/debug kernels,
  not release [XNU]. SIP posture: unrecorded in the spike — unknown, do not
  claim [M-07].
- **One genuine loose end**: the prior RAWPMU probe read configuration
  before `kpc_force_all_ctrs_set(1)`; XNU can hide RAWPMU config until
  force-all is held, so "RAWPMU 0/0" is a probe flaw, not evidence. The
  corrected probe (§5.3) is the only thing left to run, capped at half a
  day. The ARM kpc PMI reload path appears to reload configurable counters
  only — a successful generic `kpc_set_period` return must not be read as
  fixed-counter PMI support [XNU].

`thread_selfcounts`: private SPI, read-only, current-thread, may return
`ENOTSUP`, includes charged kernel work — heuristic only (E3).

### 2.4 Structural absences

No `perf_event_open` analogue; no public per-thread PMI with delivery
contract; no work-deadline run; no dirty log; no guest PMU; no counter
slope control; no documented exact single-step retirement contract;
Virtualization.framework has no vCPU surface at all.

---

## 3. Alternatives

Tiers: **T0** ≤1.4× native-virt, **T1** 1.4–3×, **T2** 3–100×, **T3**
>10³×. All figures are planning envelopes until measured.

### B. Guest software work clock (`SW_EDGE_v1`) on direct-EL1 HVF  ← primary

The inversion that dissolves the Apple dependence: stop asking the host to
observe guest work; make the guest carry its work clock as architectural
state. macOS then only needs to (a) run the guest, (b) deliver synchronous
`HVC`/`SVC`-class exits at exact instruction boundaries — architecturally
guaranteed — and (c) let us read/write guest state at stops. All three are
public and verified (§2.1).

- **Unit.** `SW_EDGE_v1` = entry into a validated instrumented execution
  chunk (basic-block-grained; a pass-enforced bound K on sites between
  consecutive deadline checks). Versioned, named, never described as or
  converted to hardware `BR_RETIRED`. V-time = f(SW_EDGE) — the same
  pure-function contract as `docs/PARAVIRT-CLOCK.md`, new unit. The
  instrumentation instructions are outside the logical clock; films bind to
  the exact guest image hash (already true of every backend).
- **Clock state — two candidate carriers, A/B'd on day one (§5.1):**
  - *Reserved-register monotonic* (v1): `x28` counter, `add x28, x28, #1`
    per chunk — single-instruction, interrupt-atomic, readable at any stop
    including CANCELED pauses; `x27` threshold. Hazard: every guest
    context-restore path (signal frames, `switch_to`, `setcontext`,
    `ptrace`, exception return) must be patched/audited to never restore
    stale x27/x28 [owned-kernel patch list; violations caught structurally
    by the differential harness].
  - *Memory-backed countdown* (cross-review): per-vCPU `remaining` cell,
    fixed always-mapped VA in every address space. Hazard (named here
    because the cross-review's sketch omits it): a naive
    `ldr/sub/str` is not atomic against interposed instrumented exception
    handlers — the write-back deterministically erases handler work and can
    clobber a host-installed deadline. The update must be single-copy
    atomic (LSE `ldadd`-class) or provably unpreemptable at every site.
  Either carrier passes or fails the same oracle; pick by evidence, not
  taste.
- **Deadline firing (correctness path — no debug architecture).** At each
  check site: decrement/compare; on zero/threshold, EL1 branches to an
  `HVC` thunk; EL0 branches to an `SVC` stub entering a patched,
  IRQ-masked EL1 trampoline that immediately executes `HVC`. Host receives
  EXCEPTION/`EC_AA64_HVC` [CITED; syndrome path day-one, M-18], reads the
  exact position, updates the V-time page, installs events, resumes at the
  continuation label. Overshoot is impossible by construction: the deadline
  is *reached by the guest*, not delivered to it. `BRK`-to-host from EL0
  (skipping the kernel transit) is a measured optimization behind M-03,
  never the baseline.
- **Injection (P2).** Only at deadline stops and synchronous traps. Either
  the cooperative kernel dispatches the event synchronously from the
  doorbell, or the VMM sets a software pending latch and reasserts
  `hv_vcpu_set_pending_interrupt` on every entry until the guest accepts
  [SDK per-run clearing]. PV event controller (shared-memory bitmap + IRQ
  pin) instead of the vGIC; GIC opaque-state restore risk avoided entirely.
  No asynchronous host kick participates in correctness; CANCELED pauses
  are invisible stops [M-11].
- **P1 inventory** (each source, its carrier): guest-EL0 counter reads
  trapped *inside the guest* via `CNTKCTL_EL1.EL0{V,P}CTEN=0`
  [VERIFIED-DOC reg access]; kernel-side `CNTVCT/CNTPCT/CNTVCTSS` reads
  made unreachable — no arch-timer clocksource, link-time encoding audit
  over every executable image; vDSO serves V-time only; PMU access
  configured to trap [SDK, M-04] *and* absent from the image by audit;
  RNDR expected absent on M-silicon [M-12] and forbidden by audit
  regardless; entropy via virtio-rng from the seeded monitor; identity
  (`MIDR/MPIDR`) pinned where settable (MPIDR is [VERIFIED-DOC]; MIDR
  M-13) else recorded in the platform fingerprint; PAC disabled in the
  owned kernel (IMPDEF QARMA out of guest-visible state); **WFI/WFE
  compiled out** into deterministic idle hypercalls — idle advances V-time
  only by the deterministic warp-to-next-deadline policy, never by host
  time; the event stream is disabled.
- **Executable closure (what invalidates counts — enforced, not assumed):**
  boot and exception assembly, vDSO, loader, all userspace, all libraries,
  and inline assembly are instrumented or statically verified; modules,
  BPF JIT, userspace JIT, and writable+executable mappings disabled or
  routed through the same verifier; alternatives/static-key/text-patching
  disabled or revalidated atomically; signal/context restoration cannot
  clobber clock state; clock loads/stores permanently resident and
  unfaultable; **LL/SC excluded — LSE-only builds** (host exits and debug
  operations can clear the exclusive monitor and change retry paths;
  `LDXR/STXR` encodings forbidden by the same audit); no instrumentation
  sequence counts itself. CAS-retry loops remain deterministic on a
  serialized-vCPU schedule (values change only at injected points).
- **Exact landing at arbitrary recorded points** (debug/branch workflows,
  not record/replay correctness): set the deadline to `W − K`, take the
  guaranteed-undershoot doorbell, then single-step (MDSCR.SS + SPSR.SS,
  step exits [VERIFIED-DOC surface]) to (SW_EDGE == W, PC == target).
  Within one W value at most one dynamic instance of any PC exists (every
  chunk entry is a site), so (W, PC) names a unique instruction instance —
  this is what makes landing sound where a bare PC breakpoint (which names
  a PC, not its k-th dynamic visit) is only an accelerator. Gated on M-03;
  until it passes, landing workflows are served by E2/C-class re-execution.
- **Snapshot/restore/branch.** At a doorbell boundary: full RAM; all
  public CPU/system/SIMD-FP/debug registers; clock state (`work`,
  `remaining`/threshold, next deadline); pending-event latches; device
  state; input-log cursor; canonical serialized hash. Dirty tracking via
  `hv_vm_protect` write-protect faults is a later accelerator [M-19];
  its absence costs speed, not correctness. Branching = restore + a
  different injected future; requires the same record-capable clock, which
  B has.
- **SMP.** One runnable vCPU at a time, quantum-interleaved by work;
  concurrent host execution of vCPUs would expose nondeterministic memory
  interleavings and is out of contract (unchanged from other backends).
- **Privilege posture.** `com.apple.security.hypervisor` only. No root,
  no SIP change, no private API.
- **Performance tier.** T0–T1 target, honestly bracketed 1.2–5×
  [M-01, M-15]; >~5× median / >~10× p99 demotes the tier, never weakens
  the contract.
- **Determinism risks.** hvf-injected guest-visible nondeterminism (the
  §5.1 kill condition); count-integrity bugs in either clock carrier
  (caught structurally by the dual-run differential); closure gaps
  (mitigated fail-closed: negative tests must fail); host-side counter
  phenomena like the M4 +73/74 are irrelevant by construction — the clock
  is guest state.
- **Fragility.** Low-to-medium: public documented API, no GIC opaque
  state, no debug dependence on the correctness path; requalify
  HVC/interrupt/step behavior per macOS update with the automated gate.
- **Verdict.** Primary spike (§5.1).

### E. Deterministic emulation (two distinct roles)

**E1 — x86-64 TCG/DBT as the product fallback.** Execute the *existing*
x86-64 Consonance guest and machine contract under single-threaded TCG on
macOS/arm64 (QEMU supports this host/guest pair [CITED: QEMU build-platform
and emulation docs]). Stock `icount` is machinery, not the clock: it
budgets translation-block instructions and is incompatible with
multithreaded TCG [CITED: QEMU icount docs]. Two honest clock options:
introduce `TCG_INSN_v1` as a new film coordinate, or — preferred —
extend the x86 translator so the budget decrements **only** on
instructions satisfying the pinned `BR_INST_RETIRED.CONDITIONAL` contract,
with the counter op executing after retirement and forcing an outer-loop
exit before the next guest instruction at zero. The event-class boundary
(`Jcc`, `LOOP*`, `JCXZ/JECXZ/JRCXZ`, macro-fusion, faulting/non-retiring
cases, model errata) must be differentially validated against the
supported Intel/KVM backend — one unexplained retirement/count/landing
mismatch kills *film compatibility* (it may proceed under a new named
clock). Stock QEMU record/replay is explicitly insufficient: it records
host clocks as nondeterministic inputs and replays them, whereas Consonance
requires same-seed independent runs to eliminate or seed those values
[CITED: QEMU replay docs]. Determinism closure: emulator-supplied
RDTSC/CPUID/MSR/RNG identity; all virtual clocks derived from branch work;
no host RTC/RNG/audio/passthrough; one TCG vCPU or explicit deterministic
schedule; pinned QEMU version, machine type, CPU model, build options,
device set; no icount auto-adjust or host-time idle warp. Posture:
ordinary process; **no** hypervisor entitlement; JIT needs
`com.apple.security.cs.allow-jit` + `MAP_JIT` [VERIFIED-DOC-class,
documented entitlement], interpreter mode avoids even that at higher cost.
Tier T2 (~10–100×; >~100× reclassifies it as an oracle, not discarded).
Apple-fragility: lowest of any option. Spike spec §5.2.

**E2 — arm64 TCG icount as the cheap differential oracle for B.** Same
payloads, third opinion in every divergence triage (B-run-1 vs B-run-2 vs
E2), and the forensic re-execution engine while M-03 is unproven. Unit
named `TCG_INSN`-class, never conflated with `SW_EDGE_v1` or hardware
events.

### C. Debug-architecture-only execution

Step exits after every putative retired instruction; host classifies
retirement, updates the authoritative count, injects due events before the
next instruction. Public surface exists [VERIFIED-DOC]; what does **not**
exist is a documented exact one-instruction retirement contract across
exceptions, IRQ acceptance, ERET, and idle [SDK] — that is the hardware
gate: [M-03] ≥10⁶ steps across exceptions, EL transitions, ERET, page
faults, pending IRQs, LSE atomics, deterministic idle; require exactly one
retirement or a precisely classified non-retirement per exit, zero
PC/PSTATE/count divergence from an interpreter; one unexplained skip,
duplicate, or livelock kills authoritative use. LL/SC unsafe here for the
same exclusive-monitor reason — LSE-only guests. Cost: direct host
stepping ~10³–10⁵×; a vEL2-hosted variant (step exceptions handled at
virtual EL2 without leaving the VM) might reach ~10²–10⁴× at the price of
nested-virt fragility [M-05] — both are estimates requiring measurement.
Role: validation oracle and slow replay engine if the gate passes; landing
subroutine for B's debug workflows; never the product path. Breakpoints do
not change the completeness argument (a PC, not its k-th visit); they
accelerate known straight-line replay segments only.

### A. Host-side kpc/kperf — closed, minus one half-day probe

Mechanism as v1 §A, now moot: the tested configurable path is guest-blind
on M4, the fixed path is guest-inclusive but unfilterably contaminated
(§2.3) [REPO-EVID][XNU]. Root-or-blessed-PID, private frameworks, per-chip
private event databases, release-kernel entitlement bypass absent: lab
posture at best, very high fragility. Remaining value: (1) the corrected
RAWPMU/force-all probe, §5.3, strictly time-boxed; (2) `thread_selfcounts`
as a free sanity heuristic beside B's authoritative clock. Kill list for
the probe: guest-flat counts, any host contamination, fixed-period no-op,
missed/duplicate overflow, unbounded skid, or overflow that does not
produce a prompt userspace vCPU exit — any one closes the route
permanently.

### D. Virtual-EL2 monitor without a PMU — closed by construction

A monitor counts only what it observes; ordinary EL0/EL1 execution between
traps is unbounded, so a trap-free loop passes any logical deadline without
the monitor regaining control. That falsifier needs no spike — it is true
by construction. The only viable completions insert per-checkpoint HVCs
(= B with a costlier per-checkpoint transition) or single-step everything
(= C, possibly cheaper under vEL2 as noted there). Re-open only if Apple
ships a nested PMU (§6).

### F. Replay-only macOS tier — folded

Replay must still answer "when has the guest reached recorded position W?"
— a position mechanism, which is exactly what A lacked and B/E/C provide.
A Linux film of `(work, event)` pairs does not make W observable on macOS
by itself; a PC breakpoint cannot name the k-th visit; a contaminated
count cannot disambiguate; an async kick can arrive after unbounded work.
Viable forms, all derivative: E1 reproducing the hardware x86 coordinate
exactly; C decoding/counting it; or Linux and macOS both running the same
instrumented image and logging `SW_EDGE_v1` (the increments are
straight-line and branch-free, so hardware branch counts on the Linux side
are unperturbed; both clocks logged at every event, each side lands on its
own). Branch-into-new-future additionally requires a record-capable clock
on macOS — i.e., B.

### G. Adjacent, honestly not macOS: Linux bare metal (Asahi lineage)

Would eventually validate the physical PMU event on M3/M4 cores, guest-only
counting under KVM, PMI delivery/skid, exact debug landing — none of the
macOS surface. Not ready: Asahi marks M3 PMU TBA / installers WIP, M4 PMU
TBA / installers unavailable [CITED: Asahi feature-support tables]. When
testable, the gate is the same 10⁶-window branch/PMI/skid/step experiment,
with the M3/M4 event resolved from that machine's database — 0x90 is an
M1/M2 fact, never silently inherited (E4).

### H. Non-starters

Virtualization.framework (no vCPU surface); unmodified-hvf wall-clock
injection (violates P2 by definition); private-framework tricks, kexts,
SIP-off research configs (fail the shipping/posture constraint).

---

## 4. Ranking

| Rank | Design | Tier (planning) | Verdict |
|---:|---|---|---|
| 1 | **B** — `SW_EDGE_v1` on direct-EL1 HVF | 1.2–5× | best product candidate; unproven; spike now |
| 2 | **E1** — x86-64 TCG, exact translated branch clock | 10–100× | credible shipping fallback + film compat; start slow path in parallel |
| 3 | **E2** — arm64 TCG icount oracle | oracle | adopt, no spike |
| 4 | **C** — debug stepping | 10²–10⁵× | oracle/slow replay behind gate M-03 |
| 5 | **A** — kpc/kperf | n/a | strong NO-GO; half-day corrected probe only |
| 6 | **F** — replay-only | derivative | folds into B/E/C |
| 7 | **D** — vEL2 sans PMU | n/a | closed by construction |
| — | **G** — Linux/Asahi on M-series | native-class, not macOS | tracked; M3/M4 PMU TBA |

The risk profile that justifies the order: B's principal risk is
executable-closure *engineering* — entirely under Consonance's control —
not an absent Apple facility; E1's principal risk is translator-contract
fidelity, entirely under Consonance's control; everything ranked below
depends on an Apple behavior that is absent, undocumented, or measured
hostile.

## 5. Spike specifications

### 5.1 Spike B — day one and week one

Minimal instrumented EL0+EL1 payload (no Linux; hand-emitted
instrumentation; both clock carriers behind a build flag): direct,
conditional, and indirect control flow; calls/returns and repeated PCs;
SVC/HVC/ERET; page faults and exception returns; masked and unmasked
pending events; LSE atomics; deterministic idle; deliberate host
cancellation under load.

For ≥10⁶ randomized deadlines, compare at every doorbell against an
independent interpreter of the same payload: `SW_EDGE`, next original PC,
PSTATE, architectural register digest, V-time page, event payload, and
event-delivery multiplicity.

Acceptance floor:

- zero count/PC/state mismatches; exactly one logical injection at every
  target, none before or after;
- identical full-state hashes across fresh processes and host-load
  conditions;
- zero divergent suffix hashes over ≥10⁴ snapshot/restore repetitions;
- CANCELED-pause storms are invisible (M-11);
- negative tests **fail closed**: an uninstrumented edge, a forbidden
  counter/RNG/`LDXR` encoding, a reserved-state clobber, a
  writable+executable page, a raw `WFI` — each must be rejected by the
  verifier or trip the harness, never pass silently;
- day-one confirmations logged: HVC syndrome path (M-18), pending-IRQ
  latch acceptance semantics (M-20), exit round-trip costs (M-15).

Kill condition: the first unexplained landing/state mismatch, or
demonstrated inability to enforce executable closure. Performance above
~5× median / ~10× p99 demotes the tier; it never weakens correctness.

### 5.2 Spike E1 — slow path first

Implement the branch-budget slow path only (no fast-path codegen tricks).
Generate cases covering every conditional-branch class, taken/not-taken,
target-fetch faults, exceptions, interrupts, atomics, MMIO, idle, and
counter/RNG instructions. For ≥10⁶ randomized deadlines: compare the TCG
count to an independent x86 decoder/interpreter; differentially compare
the same code against the supported Intel/KVM PMU backend; require exit
after the target branch and before the next guest instruction; compare
full CPU/RAM/device/event hashes across fresh processes; restore ≥10⁴
snapshots requiring identical suffixes; perturb host RTC, load, and
arrival ordering requiring unchanged guest state. One unexplained
retirement/count/landing mismatch kills film compatibility with
hardware-counter films (a fresh `TCG_INSN_v1` coordinate may still
proceed); >~100× reclassifies to oracle.

### 5.3 Probe A — half day, only if a rooted M4 is already available

On stock M3/M4/M4 Pro: record model, build, UID, signature, entitlements,
`csrutil status`, event-database hash, core type. As root:
`kpc_force_all_ctrs_set(1)` and require readback 1 **before** enumerating
FIXED/CONFIGURABLE/POWER/RAWPMU config and counts; save/restore all prior
config on every exit path. Host userspace branch-loop positive control
(zero errors over ≥10⁶ windows), then analytical guest EL1 loops N ∈
{0, 10³, 10⁶, 10⁷} requiring exact Δ=N with named privilege filtering.
Test period/action delivery separately per counter class — the ARM reload
path appears configurable-only [XNU], so a generic `kpc_set_period`
success is not fixed-PMI evidence. Require overflow to return
`hv_vcpu_run` to userspace before the guest passes the target (an
in-kernel sample with transparent re-entry is useless), then measure loss,
duplication, skid, and prove an early-arm margin + debug landing never
overshoots. Kill on any item in §3A's list. Regardless of outcome, B and
E1 proceed; this probe can only add a lab heuristic or close the file.

### 5.4 Standing oracle E2

Same 5.1 payload under `qemu-system-aarch64 -accel tcg -icount
shift=…,sleep=off`; assert the same architectural trajectory; wire into CI
as the third opinion for divergence triage.

## 6. What Apple would have to ship

**Exists, but gated/private or insufficient:**

| Facility | Limitation |
|---|---|
| `thread_selfcounts` fixed instruction/cycle accounting | private SPI, read-only, unfilterable, includes charged kernel work |
| kpc configurable counters + kpep event databases | private, root-owned; tested configurable counters **guest-blind** on M4 |
| kpc period/action sampling | private in-kernel sampling; no documented vCPU-return delivery |
| RAWPMU configuration class | private/root; corrected enumeration unmeasured; exposed registers exclude fixed `PMCR1` |
| Public debug-exception trapping | no documented exact one-retired-instruction contract |
| GIC opaque state save/restore | restore may fail across OS versions by contract |
| `hv_vm_protect` | snapshot accelerator, not a dirty log |

Note the compound requirement the guest-blind finding exposes: **opening
kpc access alone would not suffice** — Apple would also have to make the
counters attribute guest execution and expose overflow to the owning VMM.

**Exists in silicon, absent in Apple's software:** precise branch counting
(E4) with any guest visibility — plain-EL1 guests get no PMU; vEL2
advertises `PMCR_EL0.N = 0` (E1); FEAT_ECV-style guest counter trapping
(silicon presence itself unknown, M-16).

**Does not exist in the supported surface (would have to be designed):**

- a per-vCPU guest-only programmable work counter with a stable named
  event ABI;
- host/guest and EL0/EL1 filtering whose semantics include
  Hypervisor.framework guest execution;
- an absolute counter deadline / "run until N guest branches";
- a dedicated `HV_EXIT_REASON_WORK_COUNTER` / PMU-overflow exit reason;
- a documented bound on PMI skid and delivery latency;
- public read/write/save/restore of counter + overflow state;
- a nonzero nested PMU bank (tested M4: zero);
- a documented exact single-instruction execution primitive across
  exceptions, IRQs, and idle;
- virtual-counter **slope** ownership (offset exists [VERIFIED-DOC]);
- a public dirty-page log (performance, not correctness).

Minimal fast-path API that would revive the hardware design: configure a
stable event + guest privilege mask; read/write/serialize the count; arm
an absolute target; return the vCPU with a dedicated exit reason before
the next guest instruction. That plus the existing debug trap makes the
proven PMU-overflow-and-step design credible on macOS.

## 7. Measurement ledger

Resolved since v1:

| # | Question | Resolution |
|---|---|---|
| M-06 | kpc EL-config surface applies to guest counting | **Negative** — configurable bank guest-blind on M4 [REPO-EVID]; filters moot |
| M-08 | Host-EL0 wrapper constant for an EL0+EL1 clock | **Mooted** by M-06 resolution |
| M-10 | kpc per-thread attribution exactness | **Mooted** for the architecture; probe-only |
| M-17 | Asahi M3/M4 KVM+PMU | **Confirmed not ready** (PMU TBA both) [CITED] |

Open:

| # | Question | Blocking? |
|---|---|---|
| M-01 | B end-to-end overhead (both clock carriers) | tier claim only |
| M-02 | WFI-exit behavior (still relevant as a backstop; baseline compiles WFI out) | no |
| M-03 | Debug step/BRK exactness: one retirement or classified non-retirement per exit, no loss/duplication, under exceptions/IRQ/idle | gates C and B's landing workflows only (v2) |
| M-04 | PMU-access trap config behavior (macOS 26 `hv_vm_config.h`) + guest PMU read semantics | P1 audit |
| M-05 | vEL2 nested trap cost (only if C-under-vEL2 pursued) | no |
| M-07 | Probe-A environment: SIP posture, Developer Mode | probe only |
| M-09 | kpc PMI delivery/skid (probe) | probe only |
| M-11 | CANCELED stop/resume invisibility under storm | **B kill-relevant** |
| M-12 | `ID_AA64ISAR0.RNDR` under hvf (expect absent; forbidden by audit regardless) | P1 audit |
| M-13 | MIDR settability | nicety |
| M-14 | Snapshot state completeness round-trip | B milestone 2 |
| M-15 | hvf exit round-trip costs (HVC, SVC→HVC path, MMIO, step) | tier math |
| M-16 | FEAT_ECV presence (silicon + vEL2 emulation) | no |
| M-18 | HVC syndrome path day-one confirmation (EC, ISS, register marshalling) | **B day-one** |
| M-19 | `hv_vm_protect` write-fault behavior for COW/dirty tracking | speed only |
| M-20 | Pending-IRQ latch: acceptance observability + reassert semantics | **B day-one** |
| M-21 | Clock-carrier A/B: reserved-register vs atomic memory countdown (cost + hazard evidence) | B day-one |

## 8. Citations

Apple documentation, fetched 2026-08-06 [VERIFIED-DOC]: as v1 —
`hv_vm_config_set_el2_enabled` (15.0+), `hv_vcpu_set_trap_debug_exceptions`
(11.0+, "debug exceptions exit the guest", ≙ `MDCR_EL2.TDE`),
`hv_exit_reason_t` (4 values), `hv_vcpu_exit_exception_t`
(syndrome/VA/IPA), `hv_vcpu_set_vtimer_offset` (≙ `CNTVOFF_EL2`),
`hv_vcpu_set_vtimer_mask`, `hv_vcpu_set_pending_interrupt`, `hv_vm_map` /
`hv_vm_protect`, `hv_sys_reg_t` contents, `hv_vcpu_config_get_feature_reg`,
`hv_gic_create`, entitlements `com.apple.security.hypervisor` and
`com.apple.security.cs.allow-jit`.

macOS 26.4.1 SDK headers [SDK, via cross-review; re-verify in CI]:
`hv_vcpu_types.h` (exit vocabulary), `hv_vcpu.h` (`hv_vcpus_exit`
cancellation semantics; `CNTVCT = mach_absolute_time() − offset`;
pending-interrupt cleared per run; debug-trap surface), `hv_vm.h`
(map/unmap/protect only), `hv_vm_config.h` (PMU-access trap
configuration), `hv_gic_state.h` (opaque state; restore may fail across
versions).

XNU at `f6217f89…` [XNU, re-verify against shipping release]:
`osfmk/arm64/monotonic_arm64.c` (fixed counters all-modes, no filter);
`osfmk/arm64/kpc.c` (RAWPMU register list excludes fixed `PMCR1`;
PMI reload path configurable-only); `bsd/kern/kern_kpc.c`,
`osfmk/kperf/kperfbsd.c`, `bsd/kern/kern_ktrace.c` (root / blessed-PID /
dev-kernel entitlement gating); `bsd/sys/resource_private.h`
(`thread_selfcounts`).

Consonance repo [REPO-EVID]: `spike/as2h-host-count` @ `500f95d`; bead
`hm-ssz` (M4 guest-blind configurable counters, contaminated fixed
counters, flawed RAWPMU H3b probe); `docs/APPLE-SILICON.md` (E1);
`docs/PARAVIRT-CLOCK.md` (V-time contract); `docs/GLOSSARY.md` (film);
`docs/MACOS-M3-BACKEND-ALTERNATIVES.md` (pre-existing draft whose
breakpoint+short-step record/replay proposals are accelerators only —
superseded by this memo's authoritative-clock designs); bead `hm-dj0`.

Third-party [CITED]: QEMU `target/arm/hvf/hvf.c` (Apple Silicon support,
`EC_AA64_HVC` PSCI conduit, WFx handling, sysreg save/restore); QEMU hvf
gdbstub series (F. Cagnin, Nov 2022, merged 8.x) — debug traps exercised
in shipping software, exactness still ours to prove (M-03); QEMU
build-platforms, emulation, icount, and record/replay documentation
(including icount's MTTCG incompatibility and replay's recorded-host-clock
model); FFmpeg `libavutil/macos_kperf.c` (root-gated kpc usage pattern);
rr on M1/M2 under Linux (event 0x90, E4); Asahi Linux M3/M4
feature-support tables (PMU TBA).

## 9. Cross-review adjudication (v1 vs the GPT memo)

Accepted from the cross-review (with the evidence that compelled it):

- **R1. kpc architecture NO-GO** — guest-blind configurable bank,
  unfilterable fixed bank, release-kernel gating [REPO-EVID][XNU]. v1's
  Spike A withdrawn; replaced by the half-day corrected probe (their
  design, adopted nearly verbatim, including the force-all-first fix and
  the configurable-only PMI-reload caveat).
- **R2. Correctness carrier off the debug channel.** HVC/SVC doorbells are
  architecturally guaranteed synchronous traps; v1's BRK-based deadline
  relied on hvf debug-exception delivery whose exactness is undocumented.
  Adopted: debug architecture now gates only landing/debug workflows
  (M-03), not record/replay.
- **R3. LL/SC exclusion** (exclusive-monitor clearing by host exits/debug
  ops changes retry paths). A real determinism leak v1 missed; adopted
  into the closure list and the encoding audit.
- **R4. Pending-IRQ per-run clearing ⇒ software latch + reassert** [SDK].
  Adopted into B's injection design and M-20.
- **R5. WFI/WFE compiled out to idle hypercalls** as baseline (v1's
  WFE-invisibility analysis retained as defense-in-depth only).
- **R6. E1 as x86-64 TCG with film compatibility** as the fallback's
  primary form, including the icount-is-not-the-clock and
  stock-replay-is-insufficient critiques and the
  `BR_INST_RETIRED.CONDITIONAL` differential-validation obligation.
- **R7. Fail-closed negative tests and the demote-don't-relax performance
  rule** adopted into 5.1/5.2 acceptance floors.
- **R8. Wishlist upgrades**: "opening kpc alone would not suffice"
  (attribution + overflow delivery required); the no-documented-single-step
  contract item; GIC opaque-state restore caveat.

Rejected or corrected, with reasons:

- **R9. The memory-countdown sketch as written has an unnamed hazard.**
  `ldr/sub/str/cbnz` on a shared per-vCPU cell is not atomic against
  interposed instrumented exception handlers: the interrupted sequence's
  write-back deterministically erases handler-accrued work and can clobber
  a host-installed deadline. Deterministic, but it corrupts the position
  semantics the host schedules against. v2 requires the update to be
  single-copy atomic (LSE `ldadd`-class) or provably unpreemptable at
  every site, and A/Bs this carrier against v1's reserved-register
  monotonic counter (single-instruction, interrupt-atomic, readable at
  arbitrary CANCELED stops — which the countdown alone is not without
  host bookkeeping) [M-21]. Their register-restore-path concern is real
  and stays a first-class obligation on the register variant.
- **R10. Their breakpoint critique does not reach v1's landing rule.**
  Correct that a PC breakpoint cannot name the k-th dynamic visit and
  that a contaminated hint cannot disambiguate — that argument kills the
  repo draft's breakpoint+short-step *record* design. v2's landing names
  points as (W, PC) with W from the authoritative software clock and
  within-window PC uniqueness by construction; breakpoints appear only as
  accelerators, consistent with their own conclusion.
- **R11. Performance envelopes.** Their 1.5–10× for B assumed the heavier
  memory-carrier instrumentation; v1's 1.1–1.4× assumed the lightest
  register carrier. v2 brackets 1.2–5× and lets M-01 decide; the
  difference changes tier labels, not the recommendation.

Independent convergence worth recording: both memos, from different
evidence bases, ranked the same two spikes first — a guest-resident
software work clock on public direct-EL1 Hypervisor.framework, and a
deterministic TCG tier — and both concluded that every hardware-assisted
path on shipping macOS fails on the same missing property: no Apple
surface attributes, filters, or delivers guest work counts to the VMM.
