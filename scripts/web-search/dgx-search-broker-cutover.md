# DGX cutover: web-search via the search-broker (force-routed, loopback SearxNG)

Bring the live DGX Matrix bot's `web-search` onto the **search-broker** so it can
reach the DGX's **loopback** SearxNG (`127.0.0.1:8888`) while
`KASTELLAN_EGRESS_FORCE_ROUTING=1` stays on. The jailed worker keeps **zero**
direct network egress; the trusted broker (host netns) holds the only route to
SearxNG. No public SearxNG, no SSRF exemption.

> Read `memory: dgx-force-routing-deploy-facts` first. The traps below come from it.

## Prerequisites
- This branch (`feat/search-broker-sidecar`) merged to `main`, and PR #439
  (web-search endpoint-derived allowlist) merged.
- A SearxNG serving JSON on the DGX loopback (`http://127.0.0.1:8888/search`).
  (The dev instance already runs there — confirm with
  `ssh dgx 'curl -s "http://127.0.0.1:8888/search?q=test&format=json" | head -c 200'`.)

## Steps (all on the DGX; drive as `ssh dgx '<cmd>'`)

1. **Deploy the branch/build.** `upgrade_from_git.sh` is hardcoded to `main`, so
   once merged:
   ```sh
   cd ~/src/kastellan && git fetch && git checkout main && git pull
   scripts/build-release.sh          # builds kastellan-worker-search-broker too
   ```
   Confirm the new binary is staged next to the others:
   `ls ~/.local/lib/kastellan/kastellan-worker-search-broker` (exe-sibling
   discovery is how `BrokerConfig::from_env(Search, ..)` finds it).

2. **Install without starting** (avoid an uncontrolled cutover):
   ```sh
   ~/.local/bin/kastellan-cli install --no-start \
     --matrix-homeserver-url https://matrix.kastellan.dev \
     --matrix-user @kastellan:matrix.kastellan.dev
   ```
   `install` regenerates the systemd unit **and** `kastellan.env` from flags.

3. **Re-add force-routing to the regenerated unit** (`plan.rs` does NOT emit it —
   a naive install silently DISABLES a core containment control):
   ```sh
   # append to ~/.config/systemd/user/kastellan-core.service under [Service]:
   #   Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1
   systemctl --user daemon-reload
   ```

4. **Append the broker-mode env** to `~/.config/kastellan/kastellan.env`
   (install regenerated it from flags, so add these after):
   ```sh
   KASTELLAN_WEB_SEARCH_ENDPOINT=http://127.0.0.1:8888/search
   KASTELLAN_WEB_SEARCH_USE_BROKER=1
   ```
   `USE_BROKER=1` makes the web-search manifest emit the broker-mode entry (empty
   `Net::Allowlist`, no direct endpoint env, `entry.broker = BrokerSpec::search`);
   the endpoint is what the **broker** forwards to (loopback http is fine — the
   broker reaches it directly in the host netns).

5. **Restart + verify** (do NOT skip the verification):
   ```sh
   systemctl --user restart kastellan-core.service
   journalctl --user -u kastellan-core.service -n 80 --no-pager
   ```
   In the log, confirm ALL of:
   - force-routing still active (the egress proxy sidecar spawns);
   - `search-broker AVAILABLE` (the `info!` line — the broker binary was discovered);
   - web-search **registers** (the `registry.loaded` row / `<tools>` includes
     `web.search`) — if it's missing, the worker binary or endpoint is
     misconfigured (fail-closed).

   **Registered ≠ functional.** web-search registers in broker mode regardless of
   whether the broker binary resolved; if `search-broker AVAILABLE` is absent, the
   worker is registered but every dispatch fail-closes (a broker-declaring worker
   with no discovered `BrokerConfig` is refused at spawn). The `AVAILABLE` line is
   the real gate — do not rely on `web.search` merely appearing in `<tools>`.

6. **Test over Matrix.** From your Matrix client, DM `@kastellan`:
   > what happened in Germany yesterday?

   Expect a `web.search` step and a real answer. If it says it can't search,
   check the log for a broker spawn failure (fail-closed: a broker-declaring
   worker with no discovered `BrokerConfig` is *refused*, so web-search would not
   have registered in step 5).

## Validate containment (optional, strong)
On the DGX, run the live e2e — it proves a non-empty results array with the
worker on an **empty** egress allowlist (zero direct reach), the broker holding
the only route:
```sh
ssh dgx 'cd ~/src/kastellan && setsid bash -lc "source ~/.cargo/env && \
  cargo build --workspace && \
  cargo test -p kastellan-core --test search_broker_egress_e2e -- --ignored --nocapture \
  > ~/search-broker-e2e.log 2>&1; echo DONE_EXIT=\$? >> ~/search-broker-e2e.log" </dev/null & echo launched'
# poll ~/search-broker-e2e.log for DONE_EXIT=0 and a non-[SKIP] run. A [SKIP]
# means no userns — install the bwrap AppArmor profile first (see CLAUDE.md).
```

## Rollback
Set `KASTELLAN_WEB_SEARCH_USE_BROKER=0` (or remove it) in `kastellan.env` and
restart. web-search falls back to the direct entry — which, under force-routing,
cannot reach the loopback SearxNG (that's the whole reason for the broker), so
rollback means web-search is non-functional until a routable endpoint or the
broker is restored. Prefer fixing forward.
