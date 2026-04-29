// Vanilla JS — no framework. Reads snapshot.json next to index.html
// and renders three sections: providers, features, schema.

const SNAPSHOT_URL = "snapshot.json";
const ONLINE_WINDOW_SECS = 600; // 10 min — provider counts as online
                                 // if any heartbeat in this window.

const $ = (sel) => document.querySelector(sel);

function fmtRelative(unix) {
  if (unix == null) return "—";
  const now = Math.floor(Date.now() / 1000);
  const diff = now - unix;
  if (diff < 0) return "in " + fmtDuration(-diff);
  if (diff < 30) return "just now";
  return fmtDuration(diff) + " ago";
}

function fmtDuration(secs) {
  if (secs < 60) return secs + "s";
  if (secs < 3600) return Math.floor(secs / 60) + "m";
  if (secs < 86400) return Math.floor(secs / 3600) + "h";
  return Math.floor(secs / 86400) + "d";
}

function fmtRate(specs) {
  if (!specs || specs.length === 0) return "—";
  const cheapest = specs.reduce((acc, s) =>
    s.rate_msats_per_sec < acc.rate_msats_per_sec ? s : acc, specs[0]);
  return cheapest.rate_msats_per_sec + " msat/s";
}

function shortNpub(npub) {
  if (!npub) return "";
  if (npub.length <= 18) return npub;
  return npub.slice(0, 8) + "…" + npub.slice(-6);
}

function isOnline(provider, now) {
  return provider.last_seen_unix != null
      && (now - provider.last_seen_unix) <= ONLINE_WINDOW_SECS;
}

async function loadSnapshot() {
  const res = await fetch(SNAPSHOT_URL, { cache: "no-cache" });
  if (!res.ok) throw new Error("snapshot.json not reachable: " + res.status);
  return res.json();
}

function renderFreshness(snapshot) {
  const generated = new Date(snapshot.generated_at * 1000);
  const ageSecs = Math.floor(Date.now() / 1000) - snapshot.generated_at;
  $("#freshness").textContent =
    `snapshot generated ${generated.toISOString()} · ${fmtDuration(ageSecs)} ago`;
  $("#schema-version").textContent = "v" + snapshot.version;
  $("#generated-at").textContent = "Generated " + generated.toISOString();
}

function renderProviders(snapshot) {
  const tbody = $("#providers-tbody");
  tbody.innerHTML = "";
  const now = Math.floor(Date.now() / 1000);

  const onlyOnline = $("#filter-online").checked;
  const onlyStaked = $("#filter-staked").checked;
  const onlyJurisdiction = $("#filter-jurisdiction").checked;

  let shown = 0, online = 0, staked = 0;
  for (const p of snapshot.providers) {
    const o = isOnline(p, now);
    const s = !!p.stake;
    if (o) online++;
    if (s) staked++;
    if (onlyOnline && !o) continue;
    if (onlyStaked && !s) continue;
    if (onlyJurisdiction && !p.jurisdiction) continue;
    shown++;

    const tr = document.createElement("tr");
    tr.innerHTML = `
      <td>
        <div class="hostname">${escapeHtml(p.hostname || "—")}</div>
        <div class="npub" title="${escapeHtml(p.npub)}">${shortNpub(p.npub)}</div>
      </td>
      <td>
        ${o ? '<span class="badge online">online</span>'
            : '<span class="badge offline">offline</span>'}
        ${p.anchor ? '<span class="badge anchor">anchor</span>' : ''}
      </td>
      <td>${fmtRelative(p.last_seen_unix)}</td>
      <td>${p.score.toFixed(2)}</td>
      <td>${p.stake
            ? `<span class="badge staked">${p.stake.effective_sats.toLocaleString()} sat</span>`
            : '—'}</td>
      <td>${p.isolation_level || '—'}</td>
      <td>${(p.specs || []).length}</td>
      <td>${fmtRate(p.specs)}</td>
      <td>${p.jurisdiction ? escapeHtml(p.jurisdiction) : '<span style="color: var(--text-dim)">opt-out</span>'}</td>
    `;
    tbody.appendChild(tr);
  }

  $("#provider-counters").innerHTML =
    `<span class="strong">${shown}</span> shown / ` +
    `<span class="strong">${snapshot.providers.length}</span> total · ` +
    `<span class="strong">${online}</span> online · ` +
    `<span class="strong">${staked}</span> staked`;
}

function renderFeatures() {
  const grid = $("#feature-grid");
  grid.innerHTML = "";
  const features = window.PAYGRESS_FEATURES || [];
  for (const f of features) {
    const div = document.createElement("div");
    div.className = "feature";
    div.innerHTML = `
      <div class="head">
        <span class="unit">${escapeHtml(f.unit)}</span>
        <span class="pr">${
          f.pr ? `<a href="https://github.com/DhananjayPurohit/Paygress/pull/${f.pr}" target="_blank">#${f.pr}</a>` : ''
        }</span>
      </div>
      <h3>${escapeHtml(f.title)}</h3>
      <p>${escapeHtml(f.summary)}</p>
      <div class="tests">✓ ${escapeHtml(f.tests)}</div>
    `;
    grid.appendChild(div);
  }
  $("#feature-counters").innerHTML =
    `<span class="strong">${features.length}</span> shipped`;
}

function renderSchema(snapshot) {
  const cards = $("#schema-cards");
  cards.innerHTML = "";

  const total = snapshot.providers.length;
  const isolations = {};
  let withJurisdiction = 0;
  let withStake = 0;
  let totalSpecs = 0;
  let cheapest = Infinity;
  for (const p of snapshot.providers) {
    isolations[p.isolation_level || "unknown"] =
      (isolations[p.isolation_level || "unknown"] || 0) + 1;
    if (p.jurisdiction) withJurisdiction++;
    if (p.stake) withStake++;
    if (p.specs) {
      totalSpecs += p.specs.length;
      for (const s of p.specs) {
        if (s.rate_msats_per_sec < cheapest) cheapest = s.rate_msats_per_sec;
      }
    }
  }

  const isolationStr = Object.entries(isolations)
    .map(([k, v]) => `${v} ${k}`)
    .join(" · ");

  const make = (label, value, sub) => {
    const div = document.createElement("div");
    div.className = "card";
    div.innerHTML = `
      <div class="label">${escapeHtml(label)}</div>
      <div class="value">${value}</div>
      <div class="sub">${escapeHtml(sub || "")}</div>
    `;
    cards.appendChild(div);
  };

  make("Receipt window", (snapshot.receipt_window_secs / 86400).toFixed(0) + "d",
       "Rolling window for the score aggregator");
  make("Isolation tags", total ? "✓" : "—", isolationStr || "no providers");
  make("Jurisdiction opt-in", `${withJurisdiction}/${total}`,
       "Geo only shown when the offer carries `location`");
  make("Staked providers", `${withStake}/${total}`,
       "Offers carrying a verifiable Cashu fidelity bond");
  make("Cheapest tier",
       cheapest === Infinity ? "—" : cheapest + " msat/s",
       "Across all advertised specs");
  make("Total specs offered", totalSpecs,
       "Sum across providers (basic / standard / premium)");
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

async function main() {
  try {
    const snapshot = await loadSnapshot();
    renderFreshness(snapshot);
    renderProviders(snapshot);
    renderFeatures();
    renderSchema(snapshot);

    for (const id of ["filter-online", "filter-staked", "filter-jurisdiction"]) {
      $("#" + id).addEventListener("change", () => renderProviders(snapshot));
    }
  } catch (e) {
    document.querySelector("main").innerHTML =
      `<section><h2>Snapshot not loaded</h2>
         <p style="color: var(--bad)">${escapeHtml(e.message)}</p>
         <p>Run <code>cargo run --release --bin paygress-snapshot --
         --out dashboard/snapshot.json</code> in the repo root and
         reload.</p>
       </section>`;
    console.error(e);
  }
}

main();
