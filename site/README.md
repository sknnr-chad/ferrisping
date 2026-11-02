# ferrisping.com

The FerrisPing website: one static page in `public/`, served by a Cloudflare Worker that has static assets and no
code of its own. The page has no scripts, cookies or analytics, and follows the system's light or dark theme.

## Preview

You need Node.js 22.18 or newer. In this folder:

```
npm install
npm run dev
```

Then open http://localhost:8787.

## Images

The screenshots in `public/img` and the icons are made from `docs/screenshots` and `crates/ferrisping/assets`.
After redrawing the screenshots, from the repository's root:

```
py site/make_images.py
```

It needs Python 3 with Pillow. Only use the synthetic renders from the `render_screenshots` test, never captures of
a real network.

## Deploy

Deploys use Cloudflare's [cf CLI](https://developers.cloudflare.com/cf/). Set `CLOUDFLARE_ACCOUNT_ID` to the account
that holds the ferrisping.com zone, check what would be uploaded, then deploy:

```
npx cf deploy --dry-run
npx cf deploy
```

`cloudflare.config.ts` attaches the Worker to ferrisping.com as a custom domain and turns off its workers.dev
address. www.ferrisping.com isn't part of the Worker: it's a proxied `AAAA 100::` record with a redirect rule in the
zone that sends visitors to https://ferrisping.com.
