# KVM backend + the road from `SharedKernel` → `AttestedResearchTier`

## Why this exists

paygress's marketing promise touches "trustworthy compute marketplace."
Today's reality is more modest: every spawn is a Docker / LXD container
on a host the operator owns. The host operator has root, the workload
runs in their kernel, and any privacy claim against that operator is
unfounded.

The wire format already encodes three isolation tiers
(`nostr::IsolationLevel`):

| Tier                       | Defends against co-tenant attacks | Defends against host operator |
| -------------------------- | --------------------------------- | ----------------------------- |
| `SharedKernel`             | ❌                                 | ❌                             |
| `DedicatedHost`            | ✅                                 | ❌                             |
| `AttestedResearchTier`     | ✅                                 | ✅                             |

Until this PR, every backend published `SharedKernel`. This PR ships
the second tier and lays out the path to the third.

## What this PR ships (`DedicatedHost`)

A new `KvmBackend` (`src/kvm.rs`) that implements `ComputeBackend` by
spawning a per-workload qemu/KVM virtual machine. Selected via
`backend_type: kvm` in the provider config; the offer it publishes
sets `isolation_level: dedicated-host` so consumers filtering by
isolation tier match it.

### Boundary

A regular VM (no SEV-SNP) closes the **kernel-shared** attack
surface:

- A workload exploiting a kernel CVE owns the VM's kernel, not the
  host's. Container-escape exploits don't apply.
- Co-tenants on the same host can't reach each other through cgroup /
  namespace bugs — they're in different VMs with different kernels,
  different memory, different device trees.
- Host kernel bugs that affect cgroup/namespace/seccomp do not affect
  the guest.

But the host operator with root on the hypervisor can still:

- Dump guest RAM via `virsh qemu-monitor-command <vm> 'pmemsave 0
  <ram> /tmp/dump.raw'`.
- Attach a debugger to the qemu process: `gdb -p <qemu-pid>`.
- Read the guest disk image (qcow2) at any time, mounted via
  `qemu-nbd` even while the VM runs.
- Tap the VM's network bridge with `tcpdump -i any`.
- Modify the guest kernel before boot.

So `DedicatedHost` is **isolation from co-tenants, not confidentiality
from the operator.** The doc-comment on `KvmBackend` says this; the
tier name says this; we should also say it on the website.

### What runs inside

Vanilla Ubuntu 22.04 cloud image with cloud-init. Consumer SSHes in
on the spawn-time host port. Killer-template Docker images (#31) are
NOT served on the KVM backend in v1: those templates assume the host
runs Docker, but the KVM guest is a fresh VM with no `dockerd`. A
follow-up PR can run cloud-init userdata that installs Docker +
the template image, but that's deferred so this PR is reviewable.

### What we deliberately did not solve in v1

1. **Templates inside the VM.** Out of scope; vanilla Ubuntu only.
2. **`template_ports` beyond SSH.** The qemu argv builder supports
   multiple `hostfwd` entries, but the consumer-facing `--port`
   surface lands with the templates-on-VMs follow-up.
3. **`data_path` encryption.** The whole VM disk is one qcow2 file;
   LUKS-on-loop (PR #47) doesn't apply directly. Consumer-encrypted
   guest disks via LUKS-inside-the-guest is a follow-up.
4. **Crash recovery.** A startup-time `pidfile-and-process-still-
   alive?` sweep that re-claims orphaned VMs lands separately.

## The road to `AttestedResearchTier` (SEV-SNP)

The KVM backend is structured so the SEV-SNP backend is a small
delta on top, not a parallel implementation. When we have an AMD
EPYC 7003+ host (Hetzner CCX13, Azure DCasv5, AWS m6a), the work is:

1. **Add SEV-SNP qemu flags.** `-machine
   confidential-guest-support=sev-snp,memory-backend=ram0` plus an
   `-object sev-snp-guest,id=sev-snp,...` definition. ~20 lines in
   `qemu_argv` behind a `confidential: bool` config knob.
2. **Measured boot chain.** Replace the cloud image with a kernel +
   initrd we measure. Boot via `-kernel` and `-initrd` rather than
   the cloud image's grub. Measurement = SHA-384 of (kernel || initrd
   || cmdline). Reproducible by the consumer locally so the
   attestation report can be verified.
3. **Attestation handshake.** Add a two-phase commit to the spawn
   protocol:
   - Phase 1: consumer sends spawn request (no Cashu token), provider
     returns an attestation report from `/dev/sev-guest`. Consumer
     verifies the AMD-PSP signature against AMD's KDS (key
     distribution service) and checks the measured boot value
     matches the kernel+initrd they expect.
   - Phase 2: consumer sends the Cashu token, provider unlocks the
     LUKS-keyed disk with the key delivered alongside it.
   - Wire format additions:
     - `EncryptedSpawnPodRequest.attestation_required: bool`
     - `AccessDetailsContent.attestation_report: Option<...>`
     - New `Phase1AttestationResponse` private message kind.
4. **Consumer-side verifier.** Pull in
   `sev` crate (`anjuna-security/sev` or `virtee/sev`) for the AMD
   attestation signature verification. ~200 lines of consumer-side
   code, mostly serde + cryptographic verification.
5. **Provider startup check.** `cat /sys/module/kvm_amd/parameters/sev_snp`
   must be `Y` (or equivalent). Fail-fast if not.

Estimated total: **~1 week of focused work, plus 2-3 days of
end-to-end validation on a real EPYC host.** The bottleneck is
having a host to develop against, not the code.

### Why we didn't ship this in this PR

We don't have a SEV-SNP-capable host. Writing the qemu invocation,
the attestation handshake, and the verifier without a host to run
them against would produce code that nobody could test. That's worse
than no code: it ships untrue claims about confidentiality. Ship the
backend that works on a normal VPS today (`DedicatedHost`); ship the
SEV-SNP one when an EPYC host is provisioned.

### Where to get a SEV-SNP host

| Provider          | Product                | Approx cost                |
| ----------------- | ---------------------- | -------------------------- |
| Hetzner           | CCX13 (AMD EPYC 7003)  | ~€30/mo, dedicated         |
| Azure             | DCasv5                 | ~$0.20/hr on demand        |
| AWS               | m6a / c7a + Nitro      | ~$0.10/hr (different model)|
| GCP               | Confidential Compute   | ~$0.12/hr                  |

Hetzner CCX13 is the dev-loop sweet spot: dedicated, predictable, no
per-second billing surprises.

## Provider operator: how to use the KVM backend

```bash
# 1. Verify your host has KVM (most bare-metal Linux + many VPSes).
ls /dev/kvm        # must exist
qemu-system-x86_64 --version

# 2. Update provider-config.json:
#    "backend_type": "Kvm"
#
#    The backend will download the Ubuntu cloud image to
#    /var/lib/paygress/vm/base/ on first spawn (~600 MB).

# 3. Restart paygress-provider.
systemctl restart paygress-provider

# 4. Verify the offer now publishes dedicated-host:
paygress-cli list info <your-npub> | grep -i isolation

# Expected: "Isolation: dedicated-host"
```

Consumers explicitly opt in to the higher isolation tier via the
existing `--isolation-level` flag (Unit 22, follow-up):

```bash
paygress-cli list --isolation-level dedicated-host
paygress-cli spawn --isolation-level dedicated-host --provider <npub> ...
```

(The consumer-side filter UI is a separate PR; today the offer
publishes the right tier and forward-compatible clients can already
filter on it.)

## Acceptance test (run on a KVM-enabled host)

```bash
# 1. Start provider with backend_type: Kvm
paygress-cli provider start --config /etc/paygress/provider-config.json

# 2. From a fresh consumer identity, spawn a VM
paygress-cli spawn --provider <npub> --token <cashu> --tier basic \
    --ssh-pass "MyPass2026"

# 3. Expected: AccessDetails returns ssh -p <port> root@<host>;
#    behind the scenes a qemu VM is running with its own kernel.

# 4. Inside the VM, prove the kernel is NOT the host's:
ssh -p <port> root@<host> 'uname -r'   # different from host's uname -r

# 5. Check isolation: try escape vectors that work on Docker but not VMs
ssh -p <port> root@<host> '
    cat /proc/1/status | grep CapEff;          # full caps inside VM
    ls /sys/fs/cgroup;                         # VM-private cgroup
    mount;                                     # VM-private mount ns
'
```
