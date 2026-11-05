---
status: accepted
date: 2026-11-05
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0111: A Low or Medium security finding that is not a blocker is a draft security advisory and does not block a release

## Context and Problem Statement

[ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
(D6) keeps the details of an unfixed vulnerability out of public issues,
pull requests, comments and commit messages, and
[ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
repeats it for the issues agents open. The review loop
([ADR-0096](0096-agents-review-on-the-local-branch-before-every-push.md),
[workflow.md](../agents/workflow.md#host-safety-fix-loop)) fixes High
findings and the cheap Medium and Low ones in the same round; a finding it
does not fix and that describes an open weakness "stays with the
maintainer". Those items had no home on GitHub: they were kept in a file
outside GitHub that only the agents' local memory held. On 2026-11-05 the
maintainer asked to change the workflow so that small security findings
are tracked in GitHub Security Advisories and, for now, bypassed so that
releases go out as fast as possible, and so that the reports kept on the
machine move there too. Three follow-up questions the same day settled
what counts as small, what happens to the fixes already in progress, and
when an advisory is published.

## Considered Options

- Keep the file outside GitHub and the loop as it is (the rule until now).
- Fix every security finding in the pull request at hand, whatever its
  severity. Nothing waits, but each finding delays the pull request and
  the release, which the maintainer wants fast.
- Track a Low or Medium finding that is not a blocker as a draft security
  advisory, and let it wait. Chosen, with the threshold the maintainer
  chose as recommended (below).

## Decision Outcome

A security finding rated Low or Medium (a CVSS 3.1 base score below 7.0)
that is not a blocker is filed as a draft repository security advisory
(GHSA), is not fixed in the pull request at hand and does not block a
release, while blockers and anything rated High or Critical are still
fixed before a push or a release.

- **What is filed.** A `host-safety` or `red-team` finding that is not a
  blocker, an accepted residual, and any security issue rated Low or
  Medium. Each finding keeps its verdict (blocker or non-blocker) and gets
  a severity (Low, Medium, High or Critical, rated with the threat model's
  [section 8](../security/threat-model.md#8-rating-findings), and a CVSS 3.1
  vector where the reporter can).
- **Bypass.** The advisory is the whole follow-up: no issue, no fix in the
  pull request at hand, no hold on a release. The maintainer chose this "for
  now" ("tạm thời"); ending it is a new ADR.
- **Who files.** `host-safety` and `red-team` report as before. The main
  session files each such finding as a draft and does not send it back to
  the implementer; the pull request mentions it only generically, if at
  all.
- **Where details live.** D6 stands: no detail of an unfixed weakness goes
  into an issue, pull request, comment, commit or doc. A draft advisory,
  visible only to the repository's admins and collaborators, is the one
  place for them.
- **What agents may do.** Create draft advisories without asking, as
  ADR-0093 lets them open `task`, `question` and `bug` issues. Publishing,
  requesting a CVE, closing or withdrawing an advisory, and opening a
  draft's temporary private fork are the maintainer's.
- **Publication.** A draft stays a draft until a release that contains its
  fix is out. An agent then prepares its text and the maintainer publishes
  it, optionally requesting a CVE. An accepted risk stays a draft, or is
  closed once the threat model documents it publicly.
- **Reports kept on the machine.** The private security reports held
  outside GitHub move into draft advisories.
- **Fixes in progress.** Fixes decided and in progress on 2026-11-05 (#32,
  #82 and #33) finish as planned; the rule applies from the next review
  on.

### Consequences

- Good, because the details of an unfixed weakness have one home on
  GitHub, private to admins and collaborators, and can become a public
  advisory, with a CVE if wanted, once a release fixes it.
- Good, because a Low or Medium finding no longer delays the pull request
  or the release.
- Bad, because a release can ship while Low or Medium weaknesses are known
  and open; users learn of one only when its advisory is published.
- Bad, because creating an advisory is a `gh api` write, which the ask
  rules of ADR-0093 prompt for, as for a commit status [inferred from those
  rules; not tried].
- Neutral, because High and Critical findings and every blocker follow the
  loop as before.

## Evidence

No experiment; the maintainer's decision and three answers on 2026-11-05.

- The request: "Sửa lại workflow: nếu có lỗi security gì nhỏ, không nghiêm
  trọng thì sẽ dùng github Security Advisories để tracking. Và tạm thời sẽ
  bypass nó để release nhanh nhất. Còn các report privatr security đang ở
  trên máy thì cũng đẩy lên đó luôn đi" (change the workflow: small,
  non-serious security issues are tracked with GitHub Security Advisories,
  bypassed for now to release as fast as possible, and the private reports
  on the machine go there too).
- What counts as small: "Low/Medium, không phải blocker (khuyến nghị)"
  (Low or Medium, not a blocker, as recommended).
- Fixes in progress: "Để agent làm xong" (let the agents finish).
- When an advisory is published: "Sau khi có bản release đã sửa (khuyến
  nghị)" (after a release that contains the fix, as recommended).
- [external] GitHub's
  [About repository security advisories](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/about-repository-security-advisories):
  a draft is for private discussion, a temporary private fork carries a
  private fix, the REST API creates, lists and updates advisories, and a
  CVE request does not make an advisory public.
- [external] FIRST's
  [CVSS v3.1 specification](https://www.first.org/cvss/v3.1/specification-document),
  qualitative rating: Low 0.1-3.9, Medium 4.0-6.9, High 7.0-8.9, Critical
  9.0-10.0.
- [code] The threat model's
  [rating table](../security/threat-model.md#8-rating-findings) says which
  class of finding is Critical, High, Medium or Low.

## More Information

- Amends [ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
  (Issues: agents may also create draft advisories) and
  [ADR-0096](0096-agents-review-on-the-local-branch-before-every-push.md)
  (the fix loop: a Low or Medium non-blocker is neither fixed nor held in
  it). Refines [ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
  (D6: details now have a home) and
  [ADR-0097](0097-agents-work-in-fresh-bounded-contexts-instead-of-forks.md)
  (`host-safety` and `red-team` findings still go to the main session).
  The rest of each stands.
- The process: [workflow.md, Security findings](../agents/workflow.md#security-findings).
  No `question` issue is answered here.
- Revisit when the maintainer ends the bypass ("for now").
