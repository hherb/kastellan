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
