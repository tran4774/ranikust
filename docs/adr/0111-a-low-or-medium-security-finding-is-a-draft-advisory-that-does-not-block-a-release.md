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
machine move there too. Follow-up questions the same day settled what
counts as small, what happens to the fixes already in progress, when an
advisory is published, what happens to the open leads of a security audit,
how a Low or Medium draft is fixed, which rating wins when two disagree,
whether an unvalidated lead blocks anything, and what happens to a branch
whose review finds a High or Critical finding in released code it does not
touch.

## Considered Options

- Keep the file outside GitHub and the loop as it is (the rule until now).
- Fix every security finding in the pull request at hand, whatever its
  severity. Nothing waits, but each finding delays the pull request and
  the release, which the maintainer wants fast.
- Track a Low or Medium finding that is not a blocker as a draft security
  advisory, and let it wait. Chosen, with the threshold and the routes the
  maintainer chose as recommended (below).

## Decision Outcome

A security finding rated Low or Medium that is not a blocker, including a
residual the maintainer accepts, is filed as a draft repository security
advisory (GHSA); it is not fixed in the pull request at hand and does not
block a release, while a blocker, and every High or Critical finding once
it is confirmed, is fixed first.

- **Rating.** Each finding keeps its verdict (blocker or non-blocker) and
  gets a severity: Low, Medium, High or Critical, from the threat model's
  [section 8](../security/threat-model.md#8-rating-findings) and a CVSS
  score where the reporter can give one. When the two disagree the higher
  decides, and a score of 7.0 or more is at least High (9.0 or more is
  Critical). No CVSS version is pinned. A confirmed High or Critical
  finding is a blocker.
- **Filed or fixed.** The draft takes a Low or Medium non-blocker, a
  residual the maintainer accepts included. A blocker, and every confirmed
  High or Critical finding, is fixed, never filed instead. A residual the
  maintainer accepts, whatever its rating, is no blocker and has no fix.
- **How each is fixed.**
  - A blocker in a branch under review: in the review loop, before the push.
  - A High or Critical finding in released code: in the draft's temporary
    private fork, which the maintainer opens (D6), where agents work as he
    directs. When a review of a branch finds it in code the branch does not
    touch, the branch still pushes and merges; only the next release waits
    for the fix.
  - A Low or Medium draft: later, through an ordinary public pull request
    with generic wording, its test staying. The diff may show the weakness
    before the release; the maintainer accepts that for this severity.
  - A residual the maintainer accepts has no fix: threat model section 6
    lists it, and may describe it.
- **Audit leads.** Each open lead of a security audit becomes a draft with
  an estimated severity. An unvalidated lead blocks nothing, whatever the
  estimate; it becomes a blocker only once confirmed as High or Critical.
  The public deliverables in `docs/security/audit/` carry a lead only once
  it is fixed, rejected or accepted. The phase-1-2 audit, already public,
  stays as it is.
- **Bypass.** A draft holds up neither a pull request nor a release. The
  maintainer chose this "for now" ("tạm thời"); ending it is a new ADR.
- **Who files.** `host-safety` and `red-team` report as before. The main
  session files each such finding as a draft and does not send it back to
  the implementer; the pull request mentions it only generically, if at
  all.
- **Where details live.** D6 stands: no detail of an unfixed weakness goes
  into an issue, pull request, comment, commit or doc, except the diff and
  test of a Low or Medium fix pull request (generic wording) and a residual
  the maintainer lists in threat model section 6. A draft advisory is the
  one place for the rest. It is visible to repository owners, organization
  owners, security managers and users with the admin role, and to the
  collaborators added to it.
- **What agents may do.** Create a draft advisory and update its text
  without asking, as ADR-0093 lets them open `task`, `question` and `bug`
  issues. Changing an advisory's state (publish, close, withdraw),
  requesting a CVE and opening a draft's temporary private fork are the
  maintainer's.
- **Publication.** A draft stays a draft until a release that contains its
  fix is out. An agent then prepares its text and the maintainer publishes
  it, optionally requesting a CVE. A residual the maintainer accepts stays
  a draft, or is closed once the threat model documents it publicly.
- **Reports kept on the machine.** The private security reports held
  outside GitHub move into draft advisories.
- **Fixes in progress.** Fixes decided and in progress on 2026-11-05 (#32,
  #82 and #33) finish as planned; the rule applies from the next review
  on.

### Consequences

- Good, because the details of an unfixed weakness have one home on
  GitHub, private to the people above, and can become a public advisory,
  with a CVE if wanted, once a release fixes it.
- Good, because a Low or Medium finding no longer delays the pull request
  or the release.
- Bad, because a release can ship while Low or Medium weaknesses are known
  and open; users learn of one only when its advisory is published.
- Bad, because the diff of a Low or Medium fix pull request may show the
  weakness before the release that carries it.
- Bad, because creating an advisory is a `gh api` write, which the ask
  rules of ADR-0093 prompt for, as for a commit status [inferred from those
  rules; not tried].
- Neutral, because every blocker and every confirmed High or Critical
  finding follows the loop as before; one in released code that a branch
  does not touch holds only the next release.

## Evidence

No experiment; the maintainer's decision and his answers on 2026-11-05.

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
- Open audit leads: "Lead mở vào GHSA nháp (khuyến nghị)" (open leads go to
  draft GHSAs, as recommended).
- How a Low or Medium draft is fixed: "Low/Medium qua PR thường (khuyến
  nghị)" (Low and Medium fixes go through ordinary pull requests, as
  recommended).
- Which rating wins: "Lấy mức cao hơn (khuyến nghị)" (the higher rating
  wins, as recommended).
- Whether an unvalidated lead blocks: "Chỉ chặn khi đã xác nhận (khuyến
  nghị)" (block only once confirmed, as recommended).
- A High or Critical finding in released code that the reviewed branch does
  not touch: "X push bình thường, release chờ (khuyến nghị)" (X pushes
  normally, the release waits, as recommended).
- [external] GitHub's
  [About repository security advisories](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/about-repository-security-advisories):
  a draft is for private discussion, a temporary private fork carries a
  private fix, the REST API creates, lists and updates advisories, and a
  CVE request does not make an advisory public.
- [external] GitHub's
  [Permission levels for repository security advisories](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/permission-levels-for-repository-security-advisories):
  repository owners, organization owners, security managers and users with
  the admin role access every advisory; only they add collaborators, create
  the temporary private fork, merge its changes, and close or publish a
  draft. A collaborator added to an advisory has write permission: it
  sees the draft and edits its metadata.
- [external] FIRST's CVSS specifications
  ([v3.1](https://www.first.org/cvss/v3.1/specification-document),
  [v4.0](https://www.first.org/cvss/v4-0/specification-document)),
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
