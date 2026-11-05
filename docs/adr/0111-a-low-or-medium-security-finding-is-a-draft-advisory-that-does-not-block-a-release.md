---
status: accepted
date: 2026-11-05
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0111: A Low or Medium security finding that is not a blocker is a draft security advisory and does not block a release

## Context and Problem Statement

[ADR-0087](0087-work-flows-through-github-issues-pull-requests-and-a-project.md)
(D6) and
[ADR-0093](0093-agents-ask-rules-are-tightened-guardrails-and-agents-may-open-question-and-bug-issues.md)
keep the details of an unfixed vulnerability out of public issues, pull
requests, comments and commit messages. The review loop
([ADR-0096](0096-agents-review-on-the-local-branch-before-every-push.md),
[workflow.md](../agents/workflow.md#host-safety-fix-loop)) fixes High
findings and the cheap Medium and Low ones in the same round; a finding it
does not fix and that describes an open weakness "stays with the
maintainer", with no home on GitHub: such reports were kept in a file
outside it. On 2026-11-05 the maintainer asked to track small security
findings in GitHub Security Advisories and, for now, bypass them so that
releases go out as fast as possible, and to move the reports kept on the
machine there too. Follow-up questions the same day settled what counts as
small, when an advisory is published, how a Low or Medium draft is fixed,
where an open audit lead goes and when it blocks, which rating wins, what
a High or Critical finding does to a reviewed branch that touches its code
and to one that does not, and where an accepted residual is recorded.

## Considered Options

- Keep the file outside GitHub and the loop as it is (the rule until now).
- Fix every security finding in the pull request at hand, whatever its
  severity. Nothing waits, but each finding delays the pull request and
  the release, which the maintainer wants fast.
- Track a Low or Medium finding that is not a blocker as a draft security
  advisory, and let it wait. Chosen, with the threshold and the routes the
  maintainer chose as recommended (Evidence).

## Decision Outcome

A security finding rated Low or Medium that is not a blocker, and a
residual the maintainer accepts whatever its rating, is filed as a draft
repository security advisory (GHSA); it is not fixed in the pull request at
hand and does not block a release, while a blocker is fixed first. The
procedure is [workflow.md, Security findings](../agents/workflow.md#security-findings);
this ADR owns the rules.

- **Rating.** Each finding has a verdict (blocker or non-blocker) and a
  severity, Low, Medium, High or Critical, from the threat model's
  [section 8](../security/threat-model.md#8-rating-findings) and a CVSS
  score where the reporter can give one. The higher of the two decides; a
  score of 7.0 or more is at least High, 9.0 or more Critical. No CVSS
  version is pinned.
- **Blocker.** A High or Critical finding is a blocker of the branch under
  review when the branch introduces it or touches the code it is in. In
  code the branch does not touch it is a non-blocker of the branch, marked
  "holds the next release": the branch pushes and merges, and only the next
  release waits for the fix. An audit lead is no blocker until it is
  confirmed as High or Critical.
- **How each is fixed.** The routing table in
  [workflow.md](../agents/workflow.md#security-findings) applies the
  answers in Evidence; this ADR records the decision, not the procedure.
  The diff of an ordinary fix may show the weakness before the release;
  the maintainer accepts that. This is a default, not a ban: the maintainer
  may have a Low or Medium finding fixed in the pull request at hand
  (Evidence).
- **Audit leads.** Each open lead of a security audit becomes a draft. The
  main session estimates its severity from section 8, since the audit
  skill gives a `needs_validation` record none. The public deliverables in
  `docs/security/audit/` carry a lead or a finding only once it is fixed
  in a release, rejected or accepted. The phase-1-2 audit, already public,
  stays as it is.
- **Bypass.** A Low or Medium draft holds up neither a pull request nor a
  release. The maintainer chose this "for now" ("tạm thời"); ending it is a
  new ADR.
- **Who files.** `host-safety`, `red-team` and `reviewer` give each security
  finding a verdict and a severity, and report a security non-blocker, and a
  blocker the private fork fixes, in full. The main session
  files it as a draft and does not send it back to the implementer.
- **Where details live.** D6 stands: no detail of an unfixed weakness goes
  into an issue, pull request, comment, commit or doc, except a fix pull
  request's diff and test, worded generically, and a residual the
  maintainer lists in threat model section 6, in general words with the
  mitigation. A draft advisory holds the rest, and so does an audit run's
  full output, a working copy kept outside the repository. A draft is
  visible to repository owners, organization owners, security managers and
  users with the admin role, and to the collaborators added to it.
- **Who may do what.** Agents create a draft advisory and update its text
  without asking, as ADR-0093 lets them open `task`, `question` and `bug`
  issues. The maintainer alone changes its state (publish, close,
  withdraw), requests a CVE, opens its temporary private fork and adds a
  collaborator to it, since that widens who sees unfixed details. A draft
  stays a draft until a release that contains its fix is out; an agent then
  prepares its text and the maintainer publishes it.
- **Transition.** The private reports held outside GitHub move into draft
  advisories. Fixes in progress on 2026-11-05 (#32, #82 and #33) finish as
  planned; the rule applies from the next review on.

### Consequences

- Good, because the details of an unfixed weakness have a home on GitHub,
  private to the people above, and can become a public advisory, with a
  CVE if wanted, once a release fixes it.
- Good, because a Low or Medium finding no longer delays the pull request
  or the release.
- Bad, because a release can ship while Low or Medium weaknesses are known
  and open; users learn of one only when its advisory is published.
- Bad, because the diff of an ordinary fix pull request may show the
  weakness before the release that carries it.
- Bad, because a branch that touches code whose finding the private fork
  fixes waits for that fix on `main`, however long it takes.
- Bad, because creating an advisory is a `gh api` write, which the ask
  rules of ADR-0093 prompt for, as for a commit status [inferred from those
  rules; not tried].
- Neutral, because a blocker the branch introduces follows the loop as
  before; a High or Critical finding in code a branch does not touch holds
  only the next release.

## Evidence

No experiment; the maintainer's decision and answers, each given on
2026-11-05. "X" is the branch under review.

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
- A High or Critical finding in released code that X does not touch: "X
  push bình thường, release chờ (khuyến nghị)" (X pushes normally, the
  release waits, as recommended). The option was labelled E.
- A High finding in code merged to `main` but in no release yet, which X
  does not touch: "Như E, sửa bằng PR thường (khuyến nghị)" (as option E,
  X pushes normally and the release waits, but an ordinary pull request
  fixes it, with no private fork, as recommended; narrowed to High by the
  Critical answer below).
- A High or Critical finding in released code that X touches: "X chờ bản
  sửa từ fork (khuyến nghị)" (X waits for the fix from the fork, as
  recommended). X does not push until the private fork's fix is merged into
  `main`, then pushes, after merging `main` into it (our addition: the
  repository never force-pushes); until then its review and notes keep the
  finding out of anything public.
- [inferred] A Critical finding in code on `main` in no release yet that X
  touches is handled as the touched, released case (X waits for the fork's
  fix on `main`, then merges `main` into it and pushes), from the answers
  on touched released code and on Critical findings: a Critical finding
  always goes through the private fork, so X's public loop cannot fix it.
  The maintainer may overrule this.
- [inferred] A High finding in code on `main` in no release yet that X
  touches is fixed in X's own loop. No answer covers it: it is the loop as
  before this ADR, and the answer on High findings in that code already
  accepts a public diff.
- Whether the High rule for code on `main` in no release yet covers
  Critical: "Critical đi qua private fork (khuyến nghị)" (Critical goes
  through the private fork, as recommended). Only a High finding in that
  code goes through an ordinary pull request; a Critical one always goes
  through the private fork, released or not. The question was asked because
  [code] [SECURITY.md](../../SECURITY.md#supported-versions) supports
  `main`: a fix lands there and ships in the next release.
- Where an accepted residual is recorded: "GHSA nháp, đóng khi §6 đã ghi
  (khuyến nghị)" (a draft GHSA, closed once section 6 records it, as
  recommended). It becomes a draft at once; the maintainer closes it once
  its entry is on `main`.
- Extra small rules (several could be chosen; only this one was): "Thêm
  người vào GHSA là việc của anh (khuyến nghị)" (adding people to an
  advisory is the maintainer's, as recommended). Like a private fork, it
  widens who sees unfixed details. The other option offered, listing the
  open drafts in the Releasing steps of `CONTRIBUTING.md`, was not chosen.
- On #82, a Low finding fixed in the pull request at hand: "Sửa luôn, coi là
  phần của quyết định (khuyến nghị)" (fix it now, as part of the decision, as
  recommended), because it completed a decision that pull request
  implements. The finding is not described here.
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
- [external] GitHub's
  [Collaborating in a temporary private fork](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/collaborating-in-a-temporary-private-fork-to-resolve-a-repository-security-vulnerability):
  "status checks do not run on pull requests in temporary private forks"
  and "integrations, including CI, cannot access temporary private forks";
  an advisory admin merges all the fork's open pull requests at once, from
  the advisory; "GitHub won't enforce any of the protection rules" on the
  target branch. So the fork's fix reaches `main` without CI and without
  the ruleset's gates.
- [external] FIRST's CVSS specifications
  ([v3.1](https://www.first.org/cvss/v3.1/specification-document),
  [v4.0](https://www.first.org/cvss/v4-0/specification-document)),
  qualitative rating: Low 0.1-3.9, Medium 4.0-6.9, High 7.0-8.9, Critical
  9.0-10.0.
- [code] The threat model's
  [rating table](../security/threat-model.md#8-rating-findings) says which
  class of finding is Critical, High, Medium or Low.
- [code] The vendored audit skill puts no severity on a `needs_validation`
  record (`.claude/skills/security-audit/VALIDATION-AND-REPORTING.md`,
  record shapes).

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
