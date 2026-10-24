# Open Ferry site

A static pre-release website: plain HTML, CSS and JavaScript, with no build step or dependencies. The routing walkthrough uses synthetic account names and timings and makes no network calls.

Preview locally: from `site/`, run `python -m http.server 8765`, then open port 8765 in your browser. Internal URLs are relative except in `404.html`, which uses `/open-ferry-ai-proxy/` paths so its assets and home links work at nested missing addresses. A custom domain would need those 404 paths changed.

From the repository root, run `python .github/scripts/site-check.py` and `python .github/scripts/site-check.py --self-test`. The workflow checks every pull request, merge group and push to main. Deployment runs only on `workflow_dispatch` from `main`, after checks pass; it uploads `site/` directly to GitHub Pages.

Deployment will fail until the owner enables Pages with its source set to GitHub Actions. Adding this workflow does not enable Pages. No deployment has been dispatched as part of this change.

When enabling Pages, restrict the `github-pages` environment to `main`.
