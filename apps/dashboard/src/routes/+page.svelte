<script lang="ts">
  type Theme = 'light' | 'dark';

  const themeStorageKey = 'aedos-theme';
  let theme = $state<Theme>('light');
  const brandLogo = $derived(theme === 'light' ? '/images/logo_small_light.png' : '/images/logo_small_dark.png');

  $effect(() => {
    loadTheme();
  });

  function loadTheme() {
    if (typeof localStorage === 'undefined') return;
    const savedTheme = localStorage.getItem(themeStorageKey);
    applyTheme(savedTheme === 'dark' ? 'dark' : 'light');
  }

  function applyTheme(nextTheme: Theme) {
    theme = nextTheme;
    if (typeof document !== 'undefined') {
      document.documentElement.dataset.theme = nextTheme;
    }
    if (typeof localStorage !== 'undefined') {
      localStorage.setItem(themeStorageKey, nextTheme);
    }
  }

  function toggleTheme() {
    applyTheme(theme === 'light' ? 'dark' : 'light');
  }
</script>

<svelte:head>
  <title>Aedos</title>
  <meta
    name="description"
    content="Aedos is a self-hosted AI moderation oracle for Nostr relays and clients."
  />
</svelte:head>

<main class="public-shell">
  <header class="topbar">
    <a class="brand" href="/" aria-label="Aedos home"><img src={brandLogo} alt="" />AEDOS</a>
    <nav>
      <a href="#policy">Policy</a>
      <a href="#integration">Integration</a>
      <a href="/login">Login</a>
      <button class="theme-toggle" type="button" onclick={toggleTheme}>{theme === 'light' ? 'Dark' : 'Light'}</button>
    </nav>
  </header>

  <section class="hero">
    <p class="eyebrow">Nostr Moderation Oracle</p>
    <h1>Aedos reviews content once and shares reusable verdicts.</h1>
    <p>
      Aedos is a self-hosted AI-powered moderation oracle for Nostr. It checks notes, hashtags,
      images, and videos, caches verdicts by event and media hash, and publishes labels that
      relays and clients can choose to trust.
    </p>
    <div class="actions">
      <a class="button primary" href="/login">Operator Login</a>
      <a class="button" href="#integration">Integration Details</a>
    </div>
  </section>

  <section class="grid" id="policy">
    <article>
      <span>AI Review</span>
      <h2>Provider Agnostic</h2>
      <p>
        Aedos can use a local deterministic test reviewer or an external AI moderation provider
        such as OpenAI. Secrets and API keys are never shown on this public page.
      </p>
    </article>
    <article>
      <span>Media</span>
      <h2>No Media Bytes Stored</h2>
      <p>
        Aedos stores URLs, hashes, metadata, and verdict summaries. Images and videos are fetched
        for review, but media bytes are not stored in Postgres.
      </p>
    </article>
    <article>
      <span>Text Tags</span>
      <h2>Hashtags And Nostr Topics</h2>
      <p>
        Text rules only match hashtags and Nostr <code>["t", "..."]</code> topic tags. Ordinary
        prose is not scanned for isolated marker words.
      </p>
    </article>
    <article>
      <span>Nostr Labels</span>
      <h2>NIP-32 Friendly</h2>
      <p>
        Verdicts can be published as NIP-32 kind <code>1985</code> label events, signed by the
        Aedos operator key so clients and relays can verify the source.
      </p>
    </article>
  </section>

  <section class="panel" id="integration">
    <div>
      <p class="eyebrow">Integration</p>
      <h2>Relays and clients can use HTTP, WebSockets, or Nostr labels.</h2>
    </div>
    <dl>
      <div>
        <dt>Scoped WebSocket</dt>
        <dd><code>/v1/ws</code></dd>
      </div>
      <div>
        <dt>Trusted Firehose</dt>
        <dd><code>/v1/ws/firehose</code></dd>
      </div>
      <div>
        <dt>Submit Check</dt>
        <dd><code>POST /v1/check</code></dd>
      </div>
      <div>
        <dt>Policy Namespace</dt>
        <dd><code>/moderation</code></dd>
      </div>
    </dl>
  </section>
</main>

<style>
  :global(*) {
    box-sizing: border-box;
  }

  :global(:root) {
    color-scheme: light;
    --bg: #fffdf8;
    --text: #080808;
    --muted: #55545c;
    --line: #55545c;
    --surface: rgba(255, 255, 255, 0.58);
  }

  :global(:root[data-theme='dark']) {
    color-scheme: dark;
    --bg: #000;
    --text: #f4f4f1;
    --muted: #b5b2ac;
    --line: #55545c;
    --surface: #080808;
  }

  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    letter-spacing: 0;
  }

  .public-shell {
    min-height: 100vh;
  }

  .topbar {
    min-height: 60px;
    border-bottom: 1px solid var(--line);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 24px;
    padding: 0 32px;
  }

  .brand {
    display: inline-flex;
    align-items: center;
    gap: 12px;
    color: var(--text);
    text-decoration: none;
    font-size: 1.35rem;
    font-weight: 900;
    letter-spacing: 0.24em;
  }

  .brand img {
    width: 34px;
    height: 34px;
    object-fit: contain;
  }

  nav {
    display: flex;
    align-items: center;
    gap: 18px;
  }

  nav a {
    color: var(--text);
    text-decoration: none;
    font-weight: 800;
  }

  button, .button {
    min-height: 36px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
    color: var(--text);
    padding: 0 16px;
    font: inherit;
    font-weight: 800;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    text-decoration: none;
  }

  .primary {
    background: var(--text);
    color: var(--bg);
    border-color: var(--text);
  }

  .hero {
    max-width: 1060px;
    margin: 0 auto;
    padding: 96px 28px 72px;
  }

  .eyebrow,
  article span {
    margin: 0 0 10px;
    color: var(--muted);
    text-transform: uppercase;
    font-size: 0.75rem;
    font-weight: 800;
  }

  h1 {
    max-width: 920px;
    margin: 0;
    font-size: clamp(2.7rem, 7vw, 6rem);
    line-height: 0.96;
    letter-spacing: 0;
  }

  h2 {
    margin: 0;
    font-size: clamp(1.4rem, 3vw, 2rem);
    letter-spacing: 0;
  }

  p {
    max-width: 760px;
    color: var(--muted);
    line-height: 1.65;
  }

  .hero > p:not(.eyebrow) {
    margin: 24px 0 0;
    font-size: 1.08rem;
  }

  .actions {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
    margin-top: 28px;
  }

  .grid {
    max-width: 1060px;
    margin: 0 auto;
    padding: 0 28px 42px;
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    border-top: 1px solid var(--line);
    border-left: 1px solid var(--line);
  }

  article {
    min-height: 220px;
    padding: 22px;
    border-right: 1px solid var(--line);
    border-bottom: 1px solid var(--line);
  }

  .panel {
    max-width: 1060px;
    margin: 0 auto;
    padding: 42px 28px 80px;
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(320px, 0.8fr);
    gap: 36px;
  }

  dl {
    margin: 0;
    border: 1px solid var(--line);
  }

  dl div {
    padding: 15px;
    border-bottom: 1px solid var(--line);
  }

  dl div:last-child {
    border-bottom: 0;
  }

  dt {
    color: var(--muted);
    font-size: 0.72rem;
    font-weight: 900;
    text-transform: uppercase;
  }

  dd {
    margin: 6px 0 0;
    font-weight: 800;
  }

  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }

  @media (max-width: 720px) {
    .topbar {
      padding: 0 16px;
      align-items: flex-start;
      flex-direction: column;
      padding-top: 14px;
      padding-bottom: 14px;
    }

    nav {
      width: 100%;
      flex-wrap: wrap;
    }

    .hero {
      padding-top: 62px;
    }

    .grid,
    .panel {
      grid-template-columns: 1fr;
    }
  }
</style>
