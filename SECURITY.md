# Security policy

This repository is the alfypops build of [git-ai](https://github.com/git-ai-project/git-ai), based on upstream v1.7.5. It is maintained separately from upstream, and the upstream maintainers do not support it.

## Supported versions

Only the latest `v*-cs.*` release published in this repository gets security fixes.

## Reporting a vulnerability

**Vulnerabilities in this fork:** report them privately through GitHub private vulnerability reporting: open the **Security** tab of this repository and choose **Report a vulnerability**. This covers anything that comes from the fork's changes, for example the `notes_push` and `notes_ref` settings, `daemon_repo_roots`, the Bash tool grace-window handling, the disabled self-update, and the release binaries published here. Do not open a public issue for a vulnerability.

**Vulnerabilities in upstream git-ai:** if the problem also affects upstream `git-ai-project/git-ai`, report it to the upstream maintainers, following their contact details in [their repository](https://github.com/git-ai-project/git-ai). We pick up upstream fixes when we rebase.

**Not sure which?** Report it here privately. We will tell you if it belongs upstream, and with your agreement we will pass it on.

Please include the release tag, your platform, steps to reproduce, and the impact you expect. We aim to acknowledge reports within 7 days.
