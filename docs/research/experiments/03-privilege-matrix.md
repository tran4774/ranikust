# Experiment 03: Privilege matrix

| | |
|---|---|
| **Status** | Complete, 2026-10-29 |
| **Question** | Which real environments (Linux distributions, Docker, Podman, Kubernetes, CI services) give ranikust overlay mode, which fall back to kaniko mode, and which allow no mode at all? |
| **Answer** | Overlay is available only where buildah or BuildKit already run: root on a host, privileged or loosened containers, rootless podman, and hosts without Ubuntu's user-namespace restriction. Default Docker, most locked-down CI pods and GKE Autopilot land in kaniko mode, and an ordinary user on Ubuntu ≥ 24.04, a PSS-restricted pod or OpenShift `restricted-v2` get no mode, so the tool must refuse with guidance. |
| **Related** | [ADR-0002](../../adr/0002-two-snapshot-modes.md), [ADR-0004](../../adr/0004-support-every-privilege-level.md); [modes-and-detection](../../design/modes-and-detection.md); experiments [01](01-overlay-semantics.md), [02](02-run-sandbox.md), [13](13-docker-rootful-vm.md) |
| **Original** | `docs-rust/poc/03-permission-matrix/` (Vietnamese, not in this repository) |

## Summary

- **"kaniko mode needs no privileges" holds only when the tool is root in a
  container.** An ordinary user on a host who cannot create a usable user
  namespace can run neither `RUN` nor `chown` in any mode. This is common:
  Ubuntu 24.04+ blocks it by default. A fourth outcome is needed: **refuse
  and print guidance**.
- **Ubuntu 24.04+ does not block `unshare(CLONE_NEWUSER)`; it allows it and
  then strips every capability.** A probe that stops at "unshare succeeded"
  is misled; it must go as far as writing `uid_map` and mounting overlay.
- **Step 3 of survey 06 §1 (mount a fresh `proc`) fails in every default
  container**, even where overlay in a user namespace works, because the
  container's `/proc` is masked. A fallback is required: bind the existing
  `/proc` into the rootfs (what BuildKit's
  `--oci-worker-no-process-sandbox` does). Without it, rootless overlay mode
  would almost never be chosen inside a container.
- **Default Docker blocks even `unshare(CLONE_NEWUSER)`** (Docker's
  documentation says otherwise, but the current `default.json` blocks it),
  and AppArmor `docker-default` adds `deny mount`. Result: kaniko mode.
  Default rootless podman differs: it can create a user namespace and mount
  overlay.
- **Root in a container does not need `newuidmap` or `/etc/subuid`.** It can
  write a multi-range `uid_map` itself, as long as each range fits inside
  one range of the parent map. `newuidmap` is needed only for an ordinary
  user on a host.
- Most locked-down CI pods (GKE Autopilot, PSS restricted, unprivileged
  Docker executors) land in kaniko mode. **Overlay exists only where
  buildah/BuildKit already run.** ranikust's distinctive value is one binary
  that works on both sides of the matrix, not "overlay everywhere".

## Questions

Checked against the [summary](../survey/00-summary.md) §1 and
[survey chapter 06](../survey/06-overlay-and-parallelism.md) §1 (section
numbers refer to the original chapters):

1. How do Ubuntu 23.10+ and 24.04+ restrict user namespaces, and is there a
   legitimate way for a static binary downloaded from GitHub to get past it?
2. What do other distributions allow by default?
3. What do Docker and Podman allow by default (seccomp, AppArmor), and which
   mode does each set of container flags produce?
4. What is possible in a Kubernetes pod (default seccomp, AppArmor, Pod
   Security Standards, pod user namespaces, managed offerings)?
5. What do common CI services provide?
6. What does rootless overlay actually require?
7. Which mode would ranikust land in per environment, and how does that
   compare with existing tools?

## Environment

- Development machine: Ubuntu 26.04.1, kernel 7.0.0-34, podman 5.7.0
  rootless, `crun`, image `debian:latest`.
- Sysctls: `apparmor_restrict_unprivileged_userns=1`,
  `apparmor_restrict_unprivileged_unconfined=1`,
  `unprivileged_userns_clone=1`, `max_user_namespaces=121509`.
- The test user owns the range `100000:65536` in `/etc/subuid`;
  `newuidmap` is setuid.
- Podman rootless attaches no AppArmor profile
  (`podman info`: `AppArmorEnabled=false`).
- No sudo was used and nothing was installed on the host.

## Method

The survey was **mostly desk research** (documentation, source code, issue
trackers, vendor blogs; all accessed 2026-10-29), backed by a few small
tests with rootless podman on the development machine (Table 11).

**Grades.** The original graded every claim: *high* (official documentation
or source code), *medium* (secondary source, or deduced from the mechanism),
*low* (reasoning without a source), and *measured* (run on this machine).
This report keeps the grade next to the evidence label:

- a claim backed by a cited source is **[external]** with its grade;
- a claim deduced without a source, including every predicted mode in
  Tables 5 and 9, is **[inferred]** with its grade;
- a claim run on this machine is **[measured]**; claims both documented and
  reproduced carry both labels.

## Results

### Ubuntu 23.10+ and 24.04+: user namespaces restricted by AppArmor

Table 1. Mechanism.

| Claim | Evidence | Source |
|---|---|---|
| 23.10 added the sysctl `kernel.apparmor_restrict_unprivileged_userns`; 24.04 turns it on by default (=1) | **[external]** high | [Launchpad #2046477](https://bugs.launchpad.net/bugs/2046477), [Ubuntu Discourse](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007) |
| An unconfined process calling `unshare(CLONE_NEWUSER)` still **succeeds**, but is moved to the `unprivileged_userns` profile, which does `audit deny capability`. Consequence: writing `uid_map` fails with EPERM, and nothing can be mounted | **[external]** high; **[measured]** | `/etc/apparmor.d/unprivileged_userns` on this machine; [Qualys](https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt) |
| Applications are allowed through a profile **attached to the binary's path** with a `userns,` rule, for example `profile podman /usr/bin/podman flags=(unconfined) { userns, }`. The `apparmor` package ships such profiles for `podman`, `buildah`, `crun`, `runc`, `rootlesskit`, `slirp4netns`, `pasta`, `chrome`, `chromium`, `code`… | **[external]** high; **[measured]** | `/etc/apparmor.d/` on this machine |
| A rootlesskit installed outside `/usr/bin` (for example by the Docker rootless script into `~/bin`) needs its own profile for that path | **[external]** high | [rootlesscontaine.rs AppArmor](https://rootlesscontaine.rs/getting-started/common/apparmor/), [Docker rootless troubleshooting](https://docs.docker.com/engine/security/rootless/troubleshoot/) |
| The workaround `aa-exec -p <profile allowing userns>` is closed when `kernel.apparmor_restrict_unprivileged_unconfined=1`. It is on here: `aa-exec -p podman` yields `podman//&unconfined`, and creating a user namespace is still forced into `unprivileged_userns` | **[external]** high; **[measured]** | [Qualys 2025](https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt), [Ubuntu Discourse](https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007) |
| The `bwrap-userns-restrict` profile for `/usr/bin/bwrap` may use user namespaces, but its children get `unpriv_bwrap` stacked on top. bwrap cannot be borrowed to gain capabilities | **[external]** medium | The profile file on this machine |

**Is there a legitimate route for a static binary downloaded from GitHub?**
None that **avoids root**. Every option needs an administrator once
(**[external]** high for all four):

1. Install a profile `/etc/apparmor.d/ranikust` for the binary's exact path
   (modelled on the `podman` profile above), then `apparmor_parser -r`.
   Recommended by Ubuntu and rootlesscontaine.rs.
2. Ship a `.deb` that installs `/usr/bin/ranikust` with a profile. This is
   what podman and buildah do.
3. `sysctl kernel.apparmor_restrict_unprivileged_userns=0`. Machine-wide;
   should not be recommended.
4. Run with `sudo`: root overlay mode.

Consequence for ranikust: on Ubuntu 24.04+, an ordinary user running
`~/bin/ranikust` gets **no mode at all**. The tool must recognise exactly
this situation (read `/proc/sys/kernel/apparmor_restrict_unprivileged_userns`,
and `/proc/self/attr/current` after `unshare`) and print options 1–4.

Table 2. Other distributions, defaults.

| Distribution | User namespaces for ordinary users | Evidence | Source |
|---|---|---|---|
| Debian 11, 12, 13 | On. `kernel.unprivileged_userns_clone=1` (Debian-specific sysctl) | **[external]** high | [bubblewrap README.Debian](https://metadata.security.debian.org/changelogs/main/b/bubblewrap/stable-security_README.Debian), [BuildKit rootless.md](https://github.com/moby/buildkit/blob/master/docs/rootless.md) |
| Debian 10 | Off; `unprivileged_userns_clone=1` must be set | **[external]** high | BuildKit rootless.md |
| Fedora, RHEL/Rocky 8, 9, 10 | On, with no Ubuntu-style restriction. With SELinux, rootless overlay needs kernel ≥ 5.13 | **[external]** high (RHEL 8+, per BuildKit docs), medium (Rocky 9/10) | BuildKit rootless.md, [Rocky docs](https://docs.rockylinux.org/guides/containers/rootless_podman_advanced/), [Podman performance.md](https://sources.debian.org/src/podman/5.7.0%2Bds2-3/docs/tutorials/performance.md) |
| RHEL/CentOS 7 | `user.max_user_namespaces=0`; must be raised | **[external]** high | BuildKit rootless.md |
| Arch (`linux`) | On | **[external]** medium | No recent official source; Gentoo/Arch forums |
| Arch `linux-hardened` | `unprivileged_userns_clone=0` | **[external]** medium | [forums.gentoo.org](https://forums.gentoo.org/viewtopic.php?p=8865062) |
| Alpine | Kernel has `CONFIG_USER_NS`, no restriction layer | **[inferred]** low | No source found; check with `zgrep CONFIG_USER_NS /proc/config.gz` |
| Bottlerocket, EKS Auto Mode | `user.max_user_namespaces=0` | **[external]** medium | [Coder nsjail/k8s](https://coder.com/docs/ai-coder/agent-firewall/nsjail/k8s), [Tines EKS](https://docs.3b.tines.com/en/articles/16595902-aws-eks), BuildKit rootless.md |

### Docker and Podman defaults

Table 3. Seccomp.

| Claim | Evidence | Source |
|---|---|---|
| Docker's current `default.json` allows `unshare`, `clone3`, `mount`, `fsopen`/`fsconfig`/`fsmount`, `move_mount`, `open_tree`, `setns` and `mount_setattr` only when the container has `CAP_SYS_ADMIN`. Without `CAP_SYS_ADMIN`, `clone` is filtered by the mask `0x7E020000`, **which includes `CLONE_NEWUSER`** | **[external]** high | [moby/profiles seccomp/default.json](https://github.com/moby/profiles/blob/main/seccomp/default.json) (downloaded and read 2026-10-29) |
| The table on docs.docker.com says "except `CLONE_NEWUSER`" / "except `unshare --user`". This **does not match** `default.json`. Bitbucket and BuildKit both record that default Docker blocks `unshare -U` | **[external]** medium | [Docker seccomp docs](https://docs.docker.com/engine/security/seccomp/), [BCLOUD-13655](https://jira.atlassian.com/browse/BCLOUD-13655), BuildKit rootless.md |
| containerd (Kubernetes `RuntimeDefault`) gates the same group of syscalls on `CAP_SYS_ADMIN` | **[external]** high | [containerd contrib/seccomp/seccomp_default.go](https://github.com/containerd/containerd/blob/main/contrib/seccomp/seccomp_default.go) |
| Podman (containers/common `seccomp.json`) allows `unshare`, `mount`, `clone`, `fsopen`… **unconditionally**; only `setns` requires `CAP_SYS_ADMIN` | **[external]** high; **[measured]** | [containers/common seccomp.json](https://github.com/containers/common/blob/main/pkg/seccomp/seccomp.json) |

Table 4. AppArmor.

| Claim | Evidence | Source |
|---|---|---|
| The templates of Docker (`docker-default`), containerd (`cri-containerd.apparmor.d`) and rootful Podman (`containers-default`) all contain `deny mount,` | **[external]** high | [moby/profiles apparmor/template.go](https://github.com/moby/profiles/blob/main/apparmor/template.go), [containerd contrib/apparmor/template.go](https://github.com/containerd/containerd/blob/main/contrib/apparmor/template.go), [containers/common apparmor template](https://github.com/containers/common/blob/main/pkg/apparmor/apparmor_linux_template.go) |
| `deny mount` still applies in a nested user namespace, so rootless overlay inside Docker needs `apparmor=unconfined` | **[external]** high | BuildKit rootless.md ("apparmor=unconfined allows filesystem mounts") |
| Rootless podman on this machine attaches no AppArmor profile (`podman info`: `AppArmorEnabled=false`) | **[measured]** | Table 11 |
| Whether `restrict_unprivileged_userns` has any effect inside a Docker container on an Ubuntu 24.04 host is unknown. `docker-default` uses `abi/3.0` and has no `userns` rule. It does not affect the conclusion, because seccomp blocks first | **[inferred]** low | Not tried |

Table 5. Outcome by container flags. Each outcome is predicted from Tables
3–4 and the measurements.

| How the container runs | User namespace | Overlay mount | Fresh `proc` | ranikust mode | Evidence |
|---|---|---|---|---|---|
| `docker run`, default | ✗ (seccomp) | ✗ (seccomp + AppArmor) | ✗ | **kaniko** | **[inferred]** high |
| `docker run --cap-add SYS_ADMIN` | ✓ | ✗ on a host with AppArmor (`deny mount`) | ✗ (masked) | kaniko on Ubuntu/COS; root overlay on a host without AppArmor | **[inferred]** medium |
| `docker run --security-opt seccomp=unconfined --security-opt apparmor=unconfined` | ✓ | ✓ in a user namespace | ✗ unless `systempaths=unconfined` is added | **rootless overlay**, `/proc` via `rbind` | **[external]** medium (BuildKit docs) |
| `docker run --privileged` | ✓ | ✓ | ✓ | **root overlay** (the workdir must be a volume) | **[inferred]** high |
| `podman run`, rootless default | ✓ | ✓ (`userxattr`, upper on a volume) | ✗ | **rootless overlay**, `/proc` via `rbind` | **[measured]** |
| `podman run --security-opt unmask=ALL` or `systempaths=unconfined` | ✓ | ✓ | ✓ | Full rootless overlay | **[measured]** |
| `podman run --privileged` (rootless) | ✓ | ✓ both directly and in a user namespace | ✓ | root overlay | **[measured]** |
| `podman run`, rootful | ✓ (seccomp allows it) | ✗ on a host with AppArmor | ✗ | kaniko on Ubuntu; unknown on an SELinux host | **[inferred]** low |

### Kubernetes

Table 6. Kubernetes facts.

| Claim | Evidence | Source |
|---|---|---|
| Kubelet `seccompDefault` is **false** by default: a pod that declares nothing runs **Unconfined**. The feature went GA in 1.27 but still has to be enabled in the kubelet configuration | **[external]** high (default false), medium (GA in 1.27) | [K8s seccomp reference](https://v1-36.docs.kubernetes.io/docs/reference/node/seccomp/), [Alibaba](https://help.aliyun.com/en/ack/ack-managed-and-ack-dedicated/security-and-compliance/runtime-security) |
| On nodes with AppArmor (Ubuntu, COS), the runtime applies the `RuntimeDefault` AppArmor profile by default, which has `deny mount` | **[external]** high | [PSS](https://kubernetes.io/docs/concepts/security/pod-security-standards/), [K8s AppArmor](https://kubernetes.io/docs/tutorials/security/apparmor) |
| PSS **baseline** forbids `privileged`, forbids adding `SYS_ADMIN`, forbids an explicitly declared seccomp `Unconfined` (leaving it empty is allowed), and allows `procMount: Default` only | **[external]** high | [PSS](https://kubernetes.io/docs/concepts/security/pod-security-standards/) |
| PSS **restricted** requires seccomp `RuntimeDefault`/`Localhost`, `runAsNonRoot: true`, and dropping `ALL` | **[external]** high | PSS |
| `procMount: Unmasked` is usable only together with `hostUsers: false` | **[external]** medium | BuildKit rootless.md |
| Pod user namespaces (`hostUsers: false`): alpha 1.25, beta 1.30, on by default 1.33, **GA 1.36** (04/2026). Needs containerd ≥ 2.0 or CRI-O ≥ 1.25, runc ≥ 1.2 or crun ≥ 1.9, kernel ≥ 6.3 (idmap on tmpfs) | **[external]** high | [K8s blog 1.36 GA](https://kubernetes.io/blog/2026/04/23/kubernetes-v1-36-userns-ga/), [K8s blog 1.33](https://kubernetes.io/blog/2025/04/25/userns-enabled-by-default/), [user-namespaces docs](https://kubernetes.io/docs/concepts/workloads/pods/user-namespaces/) |
| In a `hostUsers: false` pod, `CAP_SYS_ADMIN` only takes effect inside the pod's user namespace. The Kubernetes blog names "unprivileged container builds" as a main use case | **[external]** high | As above |
| CERN runs rootless buildah (overlay) and BuildKit (`privileged: true` + `hostUsers: false`) in pods. Rootless BuildKit does not run in a pod user namespace because rootlesskit needs to create a uid map | **[external]** medium | [CERN blog 2025-06](https://kubernetes.web.cern.ch/blog/2025/06/19/rootless-container-builds-on-kubernetes/) |
| GKE Autopilot: enforces seccomp `RuntimeDefault` (switching to `Unconfined` is allowed), forbids privileged (except partner/allowlisted workloads, ≥ 1.35), no `SYS_ADMIN`, **rejects** `procMount: Unmasked` since 1.33, COS nodes with AppArmor `docker-default` | **[external]** high | [Autopilot security](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/autopilot-security) |
| OpenShift: user namespaces are Tech Preview in 4.17; 4.20+ has the `restricted-v3` SCC, and 4.21 adds `nested-container` for user-namespace pods. The default `restricted-v2` runs a random UID, not root | **[external]** medium | [OKD 4.20 userns](https://docs.okd.io/4.20/nodes/pods/nodes-pods-user-namespaces.html), [RH Dev Spaces nested](https://developers.redhat.com/articles/2024/12/02/enable-nested-containers-openshift-dev-spaces-user-namespaces) |
| EKS AL2023 AMI for 1.36: kernel 6.18, containerd 2.2.3, runc 1.3.4, so `hostUsers: false` works | **[external]** medium | [AWS builder post](https://builder.aws.com/content/3F7xMMfym9HPoYxY8aYz7dMkY2w/amazon-eks-and-amazon-eks-distro-now-support-kubernetes-136) |

**Is overlay in a user namespace feasible in an unprivileged pod?** Yes, but
narrowly, and only when all four conditions hold:

1. Seccomp `Unconfined`, or undeclared while the kubelet keeps
   `seccompDefault=false`. So not under PSS restricted.
2. A node without AppArmor, or a pod that declares AppArmor `Unconfined`.
   Baseline forbids the latter.
3. `user.max_user_namespaces > 0`. Bottlerocket and EKS Auto Mode fail this.
4. A fresh `proc` cannot be mounted (masked), so `/proc` must be bound with
   `rbind`.

Conditions 1–3 can all be met, but this was not tried on a real cluster:
**[inferred]** low. With `hostUsers: false` plus `privileged: true` (how CERN
runs BuildKit), root overlay inside the pod's user namespace is possible.
That violates PSS baseline.

### Common CI services

Table 7. CI environments.

| Environment | Facts | Evidence | Source |
|---|---|---|---|
| GitHub Actions `ubuntu-24.04`, `ubuntu-22.04` | Fresh VM per job. **Passwordless sudo** | **[external]** high | [GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) |
| GitHub Actions `ubuntu-24.04`: AppArmor restriction | Many projects run `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, so it is very likely on. GitHub has no official documentation | **[external]** medium | [runner-images #10443](https://github.com/actions/runner-images/issues/10443), [Playwright commit](https://code.gitlink.org.cn/kyfx/playwright/commit/01ba528904714e02bdf19bee436988dc7d2f30eb), [Lix #545](https://git.lix.systems/lix-project/lix/issues/545) |
| GitHub Actions `ubuntu-slim` (1 CPU) | Runs in an unprivileged container; cannot mount | **[external]** high | GitHub-hosted runners |
| GitHub Actions job with `container:` | A default Docker container | **[inferred]** medium | Deduced from Tables 3–5 |
| GitLab.com hosted runners `saas-linux-*` | Dedicated VM per job, **privileged** (to run DinD); runners tagged `gitlab-org` are not | **[external]** high | [GitLab hosted runners Linux](https://docs.gitlab.com/ci/runners/hosted_runners/linux/) |
| GitLab Kubernetes executor (Helm chart) | `privileged: false` by default | **[external]** medium | [GitLab runner chart](https://archives.docs.gitlab.com/16.0/charts/charts/gitlab/gitlab-runner) |
| Jenkins Kubernetes plugin, Tekton, Argo | Just pod templates; the cluster decides privileges. Common examples use kaniko, or rootless BuildKit with `--oci-worker-no-process-sandbox`. The OpenShift Pipelines buildah task uses `vfs`, or privileged for overlay | **[external]** medium | [OpenShift Pipelines security context](https://docs.redhat.com/en/documentation/red_hat_openshift_pipelines/1.21/html/securing_openshift_pipelines/configuring-security-context-for-pods), [GitLab BuildKit](https://docs.gitlab.com/ci/docker/using_buildkit/) |
| Bitbucket Pipelines Cloud (default runtime) | Forbids `--privileged`, `--cap-add`, `--security-opt`, `--userns` on `docker run`. The daemon uses userns-remap. Self-hosted runners are unrestricted | **[external]** high | [Bitbucket docker commands](https://support.atlassian.com/bitbucket-cloud/docs/run-docker-commands-in-bitbucket-pipelines/) |
| Bitbucket Runtime v3 (opt-in) | Allows privileged containers through the Docker service | **[external]** high | [Runtime v3](https://support.atlassian.com/bitbucket-cloud/docs/enable-and-use-runtime-v3/) |
| CircleCI Docker executor | Not privileged; Remote Docker runs on a separate VM | **[external]** medium | [CircleCI docs](https://circleci.com/docs/guides/execution-managed/building-docker-images/), [discuss](https://discuss.circleci.com/t/option-to-run-docker-with-privileged-on-circle-2-0/12377) |
| CircleCI machine executor | VM; `docker --privileged` works | **[external]** medium | As above |

### What rootless overlay actually requires

Table 8. Requirements.

| Claim | Evidence | Source |
|---|---|---|
| Kernel 5.11 allows overlay mounts in a user namespace with the `userxattr` option (`user.overlay.*` xattrs instead of `trusted.overlay.*`) | **[external]** high | [overlayfs pull 5.11](https://lore.kernel.org/lkml/20201217142025.GB1236412%40miu.piliscsaba.redhat.com/), [kernel overlayfs.rst](https://docs.kernel.org/filesystems/overlayfs.html) |
| Ubuntu kernels (since about 2015) and Debian 10 (< 5.11) carry their own patch allowing overlay in a user namespace without `userxattr`. So detection must **probe at run time**, not compare versions | **[external]** high | [moby overlayutils/userxattr.go](https://github.com/moby/moby/blob/v22.06.0-beta.0/daemon/graphdriver/overlayutils/userxattr.go) |
| With SELinux, rootless overlay needs kernel ≥ 5.13. Podman ≥ 3.1 uses native overlay when it can, otherwise `fuse-overlayfs`, then `vfs` | **[external]** high | [Podman performance.md](https://sources.debian.org/src/podman/5.7.0%2Bds2-3/docs/tutorials/performance.md), [Red Hat blog](https://www.redhat.com/en/blog/podman-rootless-overlay) |
| Podman used `fuse-overlayfs` because before 5.11 an ordinary user could not mount overlay. FUSE in a user namespace exists since kernel 4.18 | **[external]** high | BuildKit rootless.md |
| The upper must support `trusted.*`/`user.*` xattrs and `d_type`. NFS and network filesystems are excluded. Overlay cannot be an upper (measured: "filesystem on … not supported as upperdir" with the upper on the container's overlay rootfs). Old ZFS breaks too | **[external]** high; **[measured]** | kernel overlayfs.rst, [Launchpad ZFS](https://bugs.launchpad.net/bugs/1873917) |
| tmpfs and ext4 work as upper in a user namespace. Whiteouts are created as ordinary char 0/0 devices | **[measured]** | Table 11 |
| Without `/etc/subuid` + `newuidmap` (setuid), an ordinary user can map only one uid. `chown` to an unmapped uid returns `EINVAL` (measured). Podman has `ignore_chown_errors=true` for HPC: every file becomes the user's, and the image may run incorrectly | **[external]** high; **[measured]** | [Red Hat: controlling access](https://www.redhat.com/en/blog/controlling-access-rootless-podman-users) |
| Root in a container can write a multi-range `uid_map`/`gid_map` for a child user namespace itself, without `newuidmap`. Conditions: a single write, and each range fits inside one range of the parent map (for example `0 0 1` + `1 1 65536`; `0 0 65536` gets EPERM because it spans two parent ranges) | **[measured]** | Table 11 |
| Rootless BuildKit needs rootlesskit + `newuidmap` + subuid; on an Ubuntu 24.04 host it also needs a profile or the sysctl turned off. In a `hostUsers: false` pod, rootlesskit cannot create the map (CERN) | **[external]** high | BuildKit rootless.md, CERN |
| Rootless buildah without subuid fails with `lchown … invalid argument`; the workaround is `ignore_chown_errors` (overlay/vfs drivers only) | **[external]** medium | Red Hat blog, [RHEL-26594](https://issues.redhat.com/browse/RHEL-26594) |

### Environment → ranikust mode

Table 9. Assumes ranikust gains two additions: an `rbind` of `/proc` when a
fresh `proc` cannot be mounted (Summary, third bullet), and writing a
multi-range map itself when it is root in a container (Summary, fifth
bullet). Without them, every "rootless overlay in a container" row becomes
"kaniko". Every row is a prediction (**[inferred]**) carrying the grade of
the facts behind it.

| Environment | Mode | Notes | Evidence |
|---|---|---|---|
| Any Linux host, run as root/sudo | **root overlay** | | **[inferred]** high |
| Ubuntu ≥ 24.04 host, ordinary user, binary in `~/bin` | **no mode; must refuse** | The user namespace is stripped of capabilities; kaniko mode needs root. Guidance: sudo, an AppArmor profile, or run in a container | **[inferred]** high; **[measured]** |
| Ubuntu ≥ 24.04 host, ordinary user, installed from a `.deb` with a profile | rootless overlay | Like podman, buildah | **[inferred]** high |
| Ubuntu 22.04, Debian 11–13, Fedora, RHEL 8+, Arch host, ordinary user | **rootless overlay** | Full uid range with `newuidmap` + `/etc/subuid`; otherwise one uid only, and multi-uid images fail on `chown` | **[inferred]** medium |
| `linux-hardened` or RHEL 7 host, ordinary user | no mode | User namespaces off | **[inferred]** medium |
| GitHub Actions `ubuntu-24.04`/`22.04` | **root overlay** via `sudo` | Without sudo: 24.04 as the Ubuntu row above; 22.04 rootless overlay | **[inferred]** high (sudo), medium (restriction) |
| GitHub Actions `ubuntu-slim` | kaniko (if root in the container) | Unknown whether it runs as root | **[inferred]** low |
| GitHub Actions job with `container:`, `docker run` default | **kaniko** | seccomp + `deny mount` | **[inferred]** high |
| Docker `--privileged` | **root overlay** | The workdir must be a volume, not on the overlay rootfs | **[inferred]** high |
| Docker `seccomp=unconfined,apparmor=unconfined` | rootless overlay | | **[inferred]** medium |
| Rootless podman, default | **rootless overlay** | Workdir on a volume; `/proc` via `rbind` | **[measured]** |
| Rootful podman, default, on Ubuntu | kaniko | `deny mount` | **[inferred]** medium |
| GitLab.com `saas-linux-*` (docker+machine, privileged) | **root overlay** | `/builds` is a volume | **[inferred]** high |
| Self-hosted GitLab docker executor, default | kaniko | `privileged=false` | **[inferred]** medium |
| Default K8s pod (GitLab k8s executor, Jenkins, Tekton, Argo) on an Ubuntu/COS node | **kaniko** | AppArmor `deny mount` | **[inferred]** medium |
| As above, on a node without AppArmor (AL2023, RHCOS, Azure Linux), seccomp undeclared | possibly rootless overlay | Not tried; SELinux may block | **[inferred]** low |
| PSS restricted pod | **no mode**, unless `hostUsers: false` | Seccomp `RuntimeDefault` blocks `unshare`; with `runAsNonRoot`, kaniko mode cannot run (it needs root) | **[inferred]** medium |
| Pod with `hostUsers: false` + `privileged: true` | root overlay (in the pod's user namespace) | Violates baseline; needs K8s ≥ 1.33, kernel ≥ 6.3, containerd 2 | **[inferred]** medium |
| GKE Autopilot | **kaniko** | `RuntimeDefault`, no privileged, `Unmasked` rejected | **[inferred]** high |
| EKS Bottlerocket / Auto Mode | **kaniko** | `max_user_namespaces=0` | **[inferred]** medium |
| AKS, Ubuntu nodes | kaniko | AppArmor | **[inferred]** medium |
| OpenShift `restricted-v2` | **no mode** | Random UID, not root. `anyuid` for kaniko mode; `restricted-v3`/`nested-container` (4.20+) for rootless overlay | **[inferred]** medium |
| Bitbucket Pipelines Cloud, default runtime | kaniko | If the build step runs as root | **[inferred]** medium |
| Bitbucket Runtime v3, inside `docker run --privileged` | root overlay | | **[inferred]** medium |
| CircleCI Docker executor / machine executor | kaniko / root overlay (sudo) | | **[inferred]** medium |

### Existing tools

Table 10. Comparable tools (**[external]**, accessed 2026-10-29).

| Tool | Runs outside a container? | Privileges needed | Overlay + fallback? | Notes | Source |
|---|---|---|---|---|---|
| **buildah** | Yes | Root, or rootless (user namespace + subuid + `newuidmap`; Ubuntu 24.04 needs the `/usr/bin/buildah` profile) | Yes: overlay → fuse-overlayfs → vfs; isolation oci/rootless/chroot | Closest to the goal. Not a single static binary (needs runc/crun for oci isolation, plus `storage.conf` and `policy.json`). No "no user namespace, no mount" path | [buildah](https://github.com/containers/buildah), [Woodpecker buildah](https://woodpecker-ci.org/plugins/buildah) |
| **BuildKit** (`buildctl-daemonless.sh`) | Yes | Root, or rootlesskit + subuid; in a container, seccomp/AppArmor unconfined or `--oci-worker-no-process-sandbox` | Overlay → fuse-overlayfs → native | A temporary daemon per build; several binaries (buildkitd, buildctl, runc, rootlesskit). Parallel DAG, the best cache | [BuildKit rootless.md](https://github.com/moby/buildkit/blob/master/docs/rootless.md), [buildctl-daemonless](https://crazymax.dev/buildkit/examples/buildctl-daemonless/) |
| **kaniko** (archived by Google 06/2025; forks by osscontainertools and Chainguard) | No (container only, as root) | Root in the container, no special capability | No overlay; sequential | Still the only option for locked-down pods | [Chainguard](https://www.chainguard.dev/unchained/fork-yeah-were-bringing-kaniko-back), [osscontainertools](https://pkg.go.dev/github.com/osscontainertools/kaniko) |
| **img** | Yes | Rootless (embedded BuildKit) | As BuildKit | Not archived, but no commit since 05/2024 (GitHub API) | [genuinetools/img](https://github.com/genuinetools/img) |
| **umoci** | Yes | None (rootless unpack/repack) | No | Does not run `RUN`, does not read Dockerfiles | [umoci](https://github.com/opencontainers/umoci) |
| **apko**, **Bazel rules_oci** | Yes | None | No | No `RUN`; assembles images from packages or artefacts | [rules_oci](https://github.com/bazel-contrib/rules_oci), [apko](https://pkg.go.dev/chainguard.dev/apko@v1.2.33) |
| **Earthly** | Yes (through BuildKit) | As BuildKit | As BuildKit | Unmaintained since 07/2025; forked as EarthBuild | README of [earthly/earthly](https://github.com/earthly/earthly) |
| **Depot** | The client does; builds run on remote builders (SaaS) | None on the client machine | N/A | Not a local build | [depot/cli](https://github.com/depot/cli) |

**What ranikust can actually add** (**[inferred]**):

1. **One static binary that picks its mode and covers both sides of the
   matrix.** buildah and BuildKit stop when there is no user namespace or no
   mount (default Docker, Autopilot, PSS). kaniko is the opposite: it never
   uses overlay, even when it could. No tool has both in one binary, with the
   same Dockerfile semantics and the same cache format.
2. **No subuid/`newuidmap` needed inside a container** (Table 8), and no
   separate `rootlesskit`/`runc`. The clearest gain is with rootless Podman
   and with Docker whose seccomp/AppArmor have been loosened.
3. **An optimised kaniko mode** (kaniko optimization notes, docs/12–28,
   external, not in this repository) for exactly the locked-down
   environments. This is a large group with no other option.
4. **Do not advertise "overlay everywhere".** In most unprivileged CI pods
   overlay is unavailable, so speed will be kaniko-mode speed. Where overlay
   is available, BuildKit already runs and is the baseline to beat.

### Tests on the development machine

Table 11. Commands run on the development machine (**[measured]**). See
Environment for versions and sysctls.

| Command | Result |
|---|---|
| `unshare -U sh -c 'cat /proc/self/attr/current; grep CapEff /proc/self/status'` | `unprivileged_userns (enforce)`, `CapEff=0` |
| `unshare -Ur id` | `write failed /proc/self/uid_map: Operation not permitted` |
| `aa-exec -p podman -- unshare -Ur id` | Same error; the label is `podman//&unconfined` |
| `podman unshare … mount -t overlay … userxattr` (upper on tmpfs) | OK, because podman has a `userns` profile |
| In default `podman run`: `mount -t overlay` directly | `permission denied` (no `CAP_SYS_ADMIN` in the container's own user namespace) |
| In default `podman run`: `unshare -Urm` + overlay with `userxattr`, upper on a tmpfs volume and on ext4 | **OK**; `rm` creates a `c 0,0` whiteout |
| As above, but with the upper on the container's rootfs (overlay) | `overlay: filesystem on … not supported as upperdir` (even with `--privileged`) |
| In default `podman run`: `unshare -Urmpf --mount-proc` | `mount /proc failed: Operation not permitted`. Also fails with `seccomp=unconfined` and with `--cap-add SYS_ADMIN` |
| As above with `--security-opt unmask=ALL` or `systempaths=unconfined` | OK |
| In default `podman run`: `unshare -Urm` + `mount --rbind /proc …` | **OK** |
| In a container: write `0 0 65536` to a child user namespace's `uid_map` | EPERM (spans two ranges of the parent map) |
| Write `0 0 1\n1 1 65536` with a single `write` call (`dd`) | **OK**, no `newuidmap` needed |
| Child user namespace mapping only `0 0 1`, `chown 12:12 a` | `Invalid argument` |
| As above with `--privileged` (rootless) | Direct overlay mount OK, fresh `proc` mount OK |

## Findings

1. An ordinary user on Ubuntu ≥ 24.04 running a binary from `~/bin` has no
   working mode; the tool needs a fourth outcome, "refuse with guidance"
   (Table 1, Table 9, Table 11).
2. On Ubuntu 24.04+, `unshare(CLONE_NEWUSER)` succeeds but the namespace has
   no capabilities, so detection must go as far as writing `uid_map` and
   mounting (Table 1, Table 11).
3. A fresh `proc` cannot be mounted in any default container, so rootless
   overlay in a container depends on an `rbind` of `/proc` (Table 5,
   Table 11).
4. Default Docker blocks `CLONE_NEWUSER` through seccomp, contrary to its
   documentation, and AppArmor adds `deny mount`: default Docker means kaniko
   mode (Table 3, Table 4).
5. Root in a container can write a multi-range `uid_map` without
   `newuidmap`, provided each range fits one parent range and the map is
   written in one call (Table 8, Table 11).
6. Most locked-down CI and managed Kubernetes environments land in kaniko
   mode; PSS restricted and OpenShift `restricted-v2` allow no mode at all
   (Table 9).
7. Run-time probing, not version comparison, is required, because Ubuntu and
   Debian patched kernels before 5.11 and SELinux raises the floor to 5.13
   (Table 8).

## Implications for ranikust

The experiment proposed these changes to survey 06 §1; they are reflected in
[ADR-0004](../../adr/0004-support-every-privilege-level.md) (detect, fall
back, or refuse with guidance; no-mode environments accepted),
[ADR-0002](../../adr/0002-two-snapshot-modes.md) and
[modes-and-detection](../../design/modes-and-detection.md):

1. Add a **"cannot run"** outcome for an ordinary user on a host without a
   usable user namespace. Do not fall back to kaniko mode with a workdir on
   the host: kaniko mode needs root for `chown` and `chroot`.
2. Probe step 1: do not trust the result of `unshare(CLONE_NEWUSER)`. Write
   the map and try a real `mount`. If
   `apparmor_restrict_unprivileged_userns=1`, print how to install a
   profile.
3. Probe step 3: if a fresh `proc` cannot be mounted, try an `rbind` of
   `/proc` (accepting the loss of a private PID namespace for `RUN`, as with
   `--oci-worker-no-process-sandbox`) instead of giving up on overlay.
4. "Needs `newuidmap` + `/etc/subuid`" is true only on a host. Root in a
   container can write a multi-range map itself (Table 8). On a host without
   subuid: either refuse multi-uid images, or offer a flag like
   `ignore_chown_errors`.
5. State explicitly that default Docker blocks **even** `CLONE_NEWUSER`
   through seccomp, on top of AppArmor `deny mount`. It is not a case of "no
   `CAP_SYS_ADMIN`, but a user namespace can be created".
6. Kernel: detect by trying a mount, not by comparing versions (Ubuntu and
   Debian patched before 5.11; SELinux needs 5.13).
7. PSS restricted + kaniko mode: `runAsNonRoot` conflicts with kaniko mode.
   Record it in [survey chapter 05](../survey/05-decisions.md) and
   [survey chapter 04](../survey/04-risks-and-roadmap.md) as a known
   limitation of kaniko (and of ranikust).

## Limitations and open questions

- Most rows are desk research. Nothing was run on Docker, Kubernetes, a CI
  service or a distribution other than the development machine's Ubuntu;
  [experiment 13](13-docker-rootful-vm.md) later tested rootful Docker in a
  VM.
- Overlay in an unprivileged pod (conditions 1–3 in the Kubernetes section)
  was not tried on a real cluster.
- The effect of `restrict_unprivileged_userns` inside a Docker container on
  an Ubuntu 24.04 host is unknown (Table 4).
- Alpine's user-namespace default has no source (Table 2).
- Whether GitHub Actions `ubuntu-slim` runs as root is unknown (Table 9).
- Rootful podman on an SELinux host is unknown (Table 5).

## Reproduction

The original is `docs-rust/poc/03-permission-matrix/README.md` (archived
outside this repository); it has no scripts. The tests of Table 11 are
single shell commands run as an ordinary user on the host or inside
`podman run` with `debian:latest`, using the flags shown in each row. The
web sources are linked in the tables; all were accessed on 2026-10-29.

## References

1. [Survey chapter 00: Summary](../survey/00-summary.md), §1.
2. [Survey chapter 06: Overlay and parallelism](../survey/06-overlay-and-parallelism.md), §1.
3. [Experiment 01: Overlay semantics](01-overlay-semantics.md).
4. [Experiment 02: RUN sandbox](02-run-sandbox.md).
5. BuildKit rootless documentation:
   <https://github.com/moby/buildkit/blob/master/docs/rootless.md>
   (accessed 2026-10-29). **[external]**
6. Docker default seccomp profile:
   <https://github.com/moby/profiles/blob/main/seccomp/default.json>
   (downloaded 2026-10-29). **[external]**
7. Ubuntu Discourse, "Understanding AppArmor user namespace restriction":
   <https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007>
   (accessed 2026-10-29). **[external]**
8. Qualys, "Three bypasses of Ubuntu unprivileged user namespace
   restrictions" (2025):
   <https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt>
   (accessed 2026-10-29). **[external]**
9. Kubernetes Pod Security Standards:
   <https://kubernetes.io/docs/concepts/security/pod-security-standards/>
   (accessed 2026-10-29). **[external]**
10. Kubernetes blog, "User namespaces GA in v1.36":
    <https://kubernetes.io/blog/2026/04/23/kubernetes-v1-36-userns-ga/>
    (accessed 2026-10-29). **[external]**
11. GKE Autopilot security:
    <https://docs.cloud.google.com/kubernetes-engine/docs/concepts/autopilot-security>
    (accessed 2026-10-29). **[external]**
12. Linux kernel overlayfs documentation:
    <https://docs.kernel.org/filesystems/overlayfs.html> (accessed
    2026-10-29). **[external]**
13. All other sources are linked inline in Tables 1–10 (accessed
    2026-10-29).
