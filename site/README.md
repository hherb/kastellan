# kastellan.dev — site source

Static site, no build step. Served by Cloudflare Pages from this directory.

## Local preview

    python3 -m http.server 8043 -d site     # from the repo root
    scripts/site/check-site.sh              # validity + link + meta checks

## Cloudflare Pages setup (operator action, one-time)

1. Cloudflare dashboard → **Workers & Pages → Create → Pages →
   Connect to Git** → select `hherb/kastellan`.
2. Build settings: Framework preset **None**, build command **(leave
   empty)**, build output directory **`site`**. Production branch
   **`main`**.
3. After the first deploy: project → **Custom domains** → add
   `kastellan.dev` (and `www.kastellan.dev`). The domain is already in
   this Cloudflare account, so DNS + TLS are provisioned automatically.

Notes: every push to `main` triggers a deploy (it's a file copy — harmless);
PRs that touch the repo get free preview URLs.

## Updating content

Content is curated by hand (see
`docs/superpowers/specs/2026-06-11-kastellan-dev-website-design.md`).
When a milestone ships, update `roadmap.html` (timeline + "Last updated"
stamp) and, if the numbers moved, the landing-page status snapshot.

## Manual site (docs.kastellan.dev) — operator setup (one-time)

The developer manual (`docs/devel/manual/*.md`) is rendered by
`scripts/site/build_manual.py` and deployed to GitHub Pages by
`.github/workflows/docs.yml` on every push to `main` that touches the manual,
the converter, or the shared styles. Local preview / verify:

    uv run scripts/site/build_manual.py --out /tmp/manual
    python3 -m http.server 8044 -d /tmp/manual
    scripts/site/check-manual.sh

One-time operator actions to bring `docs.kastellan.dev` live:

1. **GitHub** → repo **Settings → Pages** → Source: **GitHub Actions**.
   After the first `docs-site` workflow run, set **Custom domain** to
   `docs.kastellan.dev` and enable **Enforce HTTPS**.
2. **Cloudflare DNS** (the zone already lives here): add
   `CNAME  docs  →  hherb.github.io`, set to **DNS only (grey cloud)** so
   GitHub can issue the Let's Encrypt certificate. Once HTTPS is enforced and
   working, the proxy (orange cloud) may optionally be re-enabled with TLS mode
   **Full (strict)**.

The build emits a `CNAME` file (`docs.kastellan.dev`), so the custom domain
stays bound across deploys.
