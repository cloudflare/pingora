# Contributing

Welcome to Pingora! Before you make a contribution, be it a bug report, documentation improvement,
pull request (PR), etc., please read and follow these guidelines.

## Start with filing an issue

More often than not, **start by filing an issue on GitHub**. If you have a bug report or feature
request, open a GitHub issue. Non-trivial PRs will also require a GitHub issue. The issue provides
us with a space to discuss proposed changes with you and the community.

Having a discussion via GitHub issue upfront is the best way to ensure your contribution lands in
Pingora. We don't want you to spend your time making a PR, only to find that we won't accept it on
a design basis. For example, we may find that your proposed feature works better as a third-party
module built on top of or for use with Pingora and encourage you to pursue that direction instead.

**You do not need to file an issue for small fixes.** What counts as a "small" or trivial fix is a
judgment call, so here's a few examples to clarify:
- fixing a typo
- refactoring a bit of code
- most documentation or comment edits

Still, _sometimes_ we may review your PR and ask you to file an issue if we expect there are larger
design decisions to be made.

## Making a PR

After you've filed an issue, you can make your PR referencing that issue number. Once you open your
PR, it will be labelled _Needs Review_. A maintainer will review your PR as soon as they can. The
reviewer may ask for changes - they will mark the PR as _Changes Requested_ and will give you
details about the requested changes. Feel free to ask lots of questions! The maintainers are there
to help you.

Once we (the maintainers) decide to accept your change, we will label your PR as _Accepted_.
Later (usually within a week or two), we will rebase your commits onto the `main` branch in a
separate PR, batched alongside other _Accepted_ commits and any internal changes. (This process
allows us to sync the state of our internal repository with the public repository.) Once your
change lands in `main`, we will close your PR.

### Caveats

Currently, internal contributions will take priority. Today Pingora is being maintained by
Cloudflare's Content Delivery team, and internal Cloudflare proxy services are a primary user of
Pingora. We value the community's work on Pingora, but the reality is that our team has a limited
amount of resources and time. We can't promise we will review or address all PRs or issues in a
timely manner.

## Conduct

Pingora and Cloudflare OpenSource generally follows the [Contributor Covenant Code of Conduct].
Violating the CoC could result in a warning or a ban to Pingora or any and all repositories in the Cloudflare organization.

[Contributor Covenant Code of Conduct]: https://github.com/cloudflare/.github/blob/26b37ca2ba7ab3d91050ead9f2c0e30674d3b91e/CODE_OF_CONDUCT.md

## Contact

If you have any questions, please reach out to [opensource@cloudflare.com](mailto:opensource@cloudflare.com).
