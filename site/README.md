# Open Ferry site

A static pre-release website: plain HTML, CSS and JavaScript, with no build step or dependencies. The routing walkthrough uses synthetic account names and timings and makes no network calls.

Preview locally: from `site/`, run `python -m http.server 8765`, then open port 8765 in your browser. All internal URLs are relative for the GitHub Pages project path.

From the repository root, run `python .github/scripts/site-check.py` and `python .github/scripts/site-check.py --self-test`. The workflow checks site changes on pull requests and pushes to main. Deployment runs only on `workflow_dispatch`, after checks pass; it uploads `site/` directly to GitHub Pages.

Deployment will fail until the owner enables Pages with its source set to GitHub Actions. Adding this workflow does not enable Pages. No deployment has been dispatched as part of this change.
