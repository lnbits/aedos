<script lang="ts">
  type Session = {
    authenticated: boolean;
    username: string | null;
    needs_setup: boolean;
  };

  type Overview = {
    total_processed: number;
    processed_today: number;
    average_processed_per_day: number;
    queued_jobs: number;
    retry_jobs: number;
    dead_letter_jobs: number;
    status_counts: Record<string, number>;
    relays: RelayStatus[];
  };

  type RelayStatus = {
    url: string;
    online: boolean;
    error: string | null;
  };

  type ImageItem = {
    sha256: string;
    url: string;
    mime_type: string | null;
    width: number | null;
    height: number | null;
    bytes: number | null;
    first_seen_at: string;
    status: string | null;
    labels: string[];
    confidence: number | null;
    source: string | null;
    model_version: string | null;
    explanation: string | null;
    verdict_created_at: string | null;
    event_ids: string[];
  };

  type ImagesResponse = {
    items: ImageItem[];
    total: number;
    page: number;
    per_page: number;
  };

  type Setting = {
    key: string;
    value: string;
    secret: boolean;
  };

  type Toast = {
    id: number;
    message: string;
    tone: 'success' | 'error' | 'info';
  };

  const settingHints: Record<string, string> = {
    DEFAULT_POLICY: 'Fallback verdict when an event cannot be fully reviewed. Usually blur_unknown or block_unknown.',
    ENABLE_ESCALATION: 'Reserved for serious incident workflows. Keep false unless you have a legal/process path in place.',
    IMAGE_FETCH_TIMEOUT_SECONDS: 'How long the worker waits when downloading an image before marking the job as failed.',
    LABEL_NAMESPACE: 'Nostr label namespace written into published moderation labels.',
    MAX_IMAGE_BYTES: 'Largest image the worker will download and review. Bigger values cost more bandwidth and AI spend.',
    MODERATION_PROVIDER: 'Image review backend. Use deterministic for local testing or openai for OpenAI moderation.',
    NOSTR_PRIVATE_KEY: 'Secret key used to sign Aedos label events so clients and relays can verify the source.',
    NOSTR_RELAYS: 'Comma-separated relays where Aedos publishes moderation labels.',
    OPENAI_API_KEY: 'OpenAI API key used only when MODERATION_PROVIDER is set to openai.',
    OPENAI_MODERATION_MODEL: 'OpenAI moderation model name used for image review.',
    QUEUE_DEAD_LETTER_MAXLEN: 'Maximum retained failed jobs in the dead-letter stream.',
    QUEUE_STREAM_MAXLEN: 'Approximate maximum retained pending/processed queue entries in Redis.',
    RATE_LIMIT_CHECKS_PER_MINUTE: 'Per-client API limit for moderation check requests.',
    WORKER_CONCURRENCY: 'Number of images the Python worker can process in parallel.'
  };

  async function api<T>(requestPath: string, options: RequestInit = {}): Promise<T> {
    const response = await fetch(requestPath, {
      ...options,
      headers: {
        'content-type': 'application/json',
        ...(options.headers ?? {})
      }
    });
    if (!response.ok) {
      const body = await response.json().catch(() => ({ error: response.statusText }));
      throw new Error(body.error ?? 'request failed');
    }
    return response.json();
  }

  let session: Session = $state({ authenticated: false, username: null, needs_setup: true });
  let activeView = $state<'dashboard' | 'images' | 'settings'>('dashboard');
  let username = $state('');
  let password = $state('');
  let confirmPassword = $state('');
  let loading = $state(true);
  let overview = $state<Overview | null>(null);
  let images = $state<ImagesResponse>({ items: [], total: 0, page: 1, per_page: 25 });
  let settings = $state<Setting[]>([]);
  let search = $state('');
  let page = $state(1);
  let perPage = $state(25);
  let selected = $state<ImageItem | null>(null);
  let reviewStatus = $state('safe');
  let reviewLabels = $state('safe');
  let reviewConfidence = $state(1);
  let reviewExplanation = $state('');
  let toasts = $state<Toast[]>([]);
  let nextToastId = 1;

  const totalPages = $derived(Math.max(1, Math.ceil(images.total / images.per_page)));
  const processedStates = $derived(
    overview ? Object.entries(overview.status_counts).sort(([a], [b]) => a.localeCompare(b)) : []
  );

  $effect(() => {
    void loadSession();
  });

  async function loadSession() {
    loading = true;
    try {
      session = await api<Session>('/admin/api/session');
      if (session.authenticated) {
        await Promise.all([loadOverview(), loadImages(), loadSettings()]);
      }
    } catch (error) {
      notify(error instanceof Error ? error.message : 'Could not reach Aedos', 'error');
    } finally {
      loading = false;
    }
  }

  async function authenticate() {
    if (session.needs_setup && password !== confirmPassword) {
      notify('Passwords do not match', 'error');
      return;
    }
    try {
      const path = session.needs_setup ? '/admin/api/setup' : '/admin/api/login';
      await api(path, {
        method: 'POST',
        body: JSON.stringify({ username, password })
      });
      password = '';
      confirmPassword = '';
      await loadSession();
    } catch (error) {
      notify(error instanceof Error ? error.message : 'Authentication failed', 'error');
    }
  }

  async function logout() {
    await api('/admin/api/logout', { method: 'POST', body: '{}' });
    overview = null;
    images = { items: [], total: 0, page: 1, per_page: 25 };
    selected = null;
    await loadSession();
  }

  async function loadOverview() {
    overview = await api<Overview>('/admin/api/overview');
  }

  async function loadImages() {
    const params = new URLSearchParams({
      q: search,
      page: String(page),
      per_page: String(perPage)
    });
    images = await api<ImagesResponse>(`/admin/api/images?${params}`);
  }

  async function loadSettings() {
    settings = await api<Setting[]>('/admin/api/settings');
  }

  async function runSearch() {
    page = 1;
    await loadImages();
  }

  function openReview(item: ImageItem) {
    selected = item;
    reviewStatus = item.status ?? 'safe';
    reviewLabels = item.labels.length ? item.labels.join(', ') : reviewStatus;
    reviewConfidence = item.confidence ?? 1;
    reviewExplanation = item.explanation ?? '';
  }

  async function saveReview() {
    if (!selected) return;
    try {
      await api(`/admin/api/images/${selected.sha256}/verdict`, {
        method: 'POST',
        body: JSON.stringify({
          status: reviewStatus,
          labels: reviewLabels.split(',').map((label) => label.trim()).filter(Boolean),
          confidence: Number(reviewConfidence),
          explanation: reviewExplanation.trim() || null
        })
      });
      notify('Verdict updated', 'success');
      selected = null;
      await Promise.all([loadOverview(), loadImages()]);
    } catch (error) {
      notify(error instanceof Error ? error.message : 'Could not update verdict', 'error');
    }
  }

  async function saveSettings() {
    const body = {
      settings: Object.fromEntries(settings.map((setting) => [setting.key, setting.value]))
    };
    try {
      await api('/admin/api/settings', {
        method: 'POST',
        body: JSON.stringify(body)
      });
      notify('Settings saved', 'success');
      await loadSettings();
    } catch (error) {
      notify(error instanceof Error ? error.message : 'Could not save settings', 'error');
    }
  }

  function notify(message: string, tone: Toast['tone'] = 'info') {
    const id = nextToastId;
    nextToastId += 1;
    toasts = [...toasts, { id, message, tone }];
    setTimeout(() => dismissToast(id), 3500);
  }

  function dismissToast(id: number) {
    toasts = toasts.filter((toast) => toast.id !== id);
  }

  function formatBytes(value: number | null) {
    if (!value) return '-';
    if (value < 1024) return `${value} B`;
    if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
    return `${(value / 1024 / 1024).toFixed(1)} MB`;
  }

  function short(value: string) {
    return value.length > 18 ? `${value.slice(0, 10)}...${value.slice(-6)}` : value;
  }
</script>

<svelte:head>
  <title>Aedos Control</title>
</svelte:head>

{#if loading}
  <main class="auth-shell">
    <div class="brand">AEDOS</div>
    <p>Loading control surface</p>
  </main>
{:else if !session.authenticated}
  <main class="auth-shell">
    <section class="auth-panel">
      <div class="brand">AEDOS</div>
      <h1>{session.needs_setup ? 'Create Admin' : 'Sign In'}</h1>
      <form onsubmit={(event) => { event.preventDefault(); void authenticate(); }}>
        <label>
          Username
          <input bind:value={username} autocomplete="username" required />
        </label>
        <label>
          Password
          <input bind:value={password} type="password" autocomplete={session.needs_setup ? 'new-password' : 'current-password'} required minlength="12" />
        </label>
        {#if session.needs_setup}
          <label>
            Confirm Password
            <input bind:value={confirmPassword} type="password" autocomplete="new-password" required minlength="12" />
          </label>
        {/if}
        <button type="submit">{session.needs_setup ? 'Create Account' : 'Enter Dashboard'}</button>
      </form>
    </section>
  </main>
{:else}
  <main class="app-shell">
    <header class="topbar">
      <div class="brand">AEDOS</div>
      <nav>
        <button class:active={activeView === 'dashboard'} onclick={() => (activeView = 'dashboard')}>Overview</button>
        <button class:active={activeView === 'images'} onclick={() => (activeView = 'images')}>Images</button>
        <button class:active={activeView === 'settings'} onclick={() => (activeView = 'settings')}>Settings</button>
      </nav>
      <button class="ghost" onclick={logout}>{session.username}</button>
    </header>

    <aside class="sidebar">
      <button class:active={activeView === 'dashboard'} onclick={() => (activeView = 'dashboard')}>⌂<span>Overview</span></button>
      <button class:active={activeView === 'images'} onclick={() => (activeView = 'images')}>▦<span>Images</span></button>
      <button class:active={activeView === 'settings'} onclick={() => (activeView = 'settings')}>⚙<span>Settings</span></button>
    </aside>

    <section class="content">
      {#if activeView === 'dashboard'}
        <div class="page-head">
          <div>
            <p class="eyebrow">Moderation Oracle</p>
            <h1>Operations</h1>
          </div>
          <button onclick={() => void Promise.all([loadOverview(), loadImages()])}>Refresh</button>
        </div>

        <section class="stats-grid">
          <article>
            <span>Total Processed</span>
            <strong>{overview?.total_processed ?? 0}</strong>
          </article>
          <article>
            <span>Processed Today</span>
            <strong>{overview?.processed_today ?? 0}</strong>
          </article>
          <article>
            <span>Daily Average</span>
            <strong>{overview?.average_processed_per_day ?? 0}</strong>
          </article>
          <article>
            <span>Retry Queue</span>
            <strong>{overview?.retry_jobs ?? 0}</strong>
          </article>
        </section>

        <section class="status-band">
          <div>
            <span>Incoming</span>
            <strong>{overview?.queued_jobs ?? 0}</strong>
          </div>
          <div>
            <span>Processing</span>
            <strong>{overview?.retry_jobs ?? 0}</strong>
          </div>
          <div>
            <span>Dead Letter</span>
            <strong>{overview?.dead_letter_jobs ?? 0}</strong>
          </div>
          {#each processedStates as [name, count]}
            <div>
              <span>{name}</span>
              <strong>{count}</strong>
            </div>
          {/each}
        </section>

        <section class="relay-panel">
          <header>
            <span>Nostr Relays</span>
            <strong>{overview?.relays?.filter((relay) => relay.online).length ?? 0}/{overview?.relays?.length ?? 0}</strong>
          </header>
          <div class="relay-list">
            {#each overview?.relays ?? [] as relay}
              <div class="relay-row" title={relay.error ?? 'websocket connected'}>
                <span class={`relay-dot ${relay.online ? 'online' : ''}`}></span>
                <span class="relay-url">{relay.url}</span>
                <span class={relay.online ? 'relay-state online' : 'relay-state'}>{relay.online ? 'online' : 'offline'}</span>
              </div>
            {/each}
            {#if !overview?.relays?.length}
              <p class="empty">No relays configured</p>
            {/if}
          </div>
        </section>
      {:else if activeView === 'images'}
        <div class="page-head">
          <div>
            <p class="eyebrow">Reviewed Media</p>
            <h1>Images</h1>
          </div>
          <form class="search" onsubmit={(event) => { event.preventDefault(); void runSearch(); }}>
            <input bind:value={search} placeholder="Search event id, SHA-256, or URL" />
            <button type="submit">Search</button>
          </form>
        </div>

        <section class="table-shell">
          <table>
            <thead>
              <tr>
                <th>Image</th>
                <th>Events</th>
                <th>Status</th>
                <th>Source</th>
                <th>Seen</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {#each images.items as item}
                <tr>
                  <td>
                    <div class="hash">{short(item.sha256)}</div>
                    <div class="muted">{item.mime_type ?? 'unknown'} · {formatBytes(item.bytes)}</div>
                  </td>
                  <td>
                    <div class="events">{item.event_ids.slice(0, 2).join(', ') || '-'}</div>
                  </td>
                  <td><span class={`pill ${item.status ?? 'unknown'}`}>{item.status ?? 'unknown'}</span></td>
                  <td>{item.source ?? '-'}</td>
                  <td>{new Date(item.first_seen_at).toLocaleString()}</td>
                  <td><button onclick={() => openReview(item)}>Review</button></td>
                </tr>
              {/each}
              {#if images.items.length === 0}
                <tr><td colspan="6" class="empty">No images yet</td></tr>
              {/if}
            </tbody>
          </table>
          <footer class="pager">
            <span>{images.total} images</span>
            <select bind:value={perPage} onchange={() => { page = 1; void loadImages(); }}>
              <option value={10}>10</option>
              <option value={25}>25</option>
              <option value={50}>50</option>
              <option value={100}>100</option>
            </select>
            <button disabled={page <= 1} onclick={() => { page -= 1; void loadImages(); }}>‹</button>
            <span>{page} / {totalPages}</span>
            <button disabled={page >= totalPages} onclick={() => { page += 1; void loadImages(); }}>›</button>
          </footer>
        </section>
      {:else}
        <div class="page-head">
          <div>
            <p class="eyebrow">Runtime Controls</p>
            <h1>Settings</h1>
          </div>
          <button onclick={saveSettings}>Save Settings</button>
        </div>

        <section class="settings-grid">
          {#each settings as setting}
            <label>
              <span>{setting.key}</span>
              <input bind:value={setting.value} type={setting.secret ? 'password' : 'text'} autocomplete="off" />
              {#if settingHints[setting.key]}
                <small>{settingHints[setting.key]}</small>
              {/if}
            </label>
          {/each}
        </section>
      {/if}
    </section>

    {#if selected}
      <button class="modal-backdrop" type="button" aria-label="Close review" onclick={() => (selected = null)}></button>
      <section class="modal-wrap" aria-modal="true" role="dialog">
        <form class="modal" onsubmit={(event) => { event.preventDefault(); void saveReview(); }}>
          <h2>Review Image</h2>
          <p class="hash full">{selected.sha256}</p>
          <a href={selected.url} target="_blank" rel="noreferrer">{selected.url}</a>
          <label>
            Verdict
            <select bind:value={reviewStatus}>
              <option value="safe">safe</option>
              <option value="warn">warn</option>
              <option value="block">block</option>
              <option value="unknown">unknown</option>
              <option value="error">error</option>
            </select>
          </label>
          <label>
            Labels
            <input bind:value={reviewLabels} />
          </label>
          <label>
            Confidence
            <input bind:value={reviewConfidence} type="number" min="0" max="1" step="0.01" />
          </label>
          <label>
            Explanation
            <textarea bind:value={reviewExplanation}></textarea>
          </label>
          <div class="actions">
            <button type="button" class="ghost" onclick={() => (selected = null)}>Cancel</button>
            <button type="submit">Save Verdict</button>
          </div>
        </form>
      </section>
    {/if}
  </main>
{/if}

{#if toasts.length}
  <aside class="toast-stack" aria-live="polite" aria-label="Notifications">
    {#each toasts as toast (toast.id)}
      <button class={`toast ${toast.tone}`} onclick={() => dismissToast(toast.id)}>
        {toast.message}
      </button>
    {/each}
  </aside>
{/if}

<style>
  :global(*) {
    box-sizing: border-box;
  }

  :global(body) {
    margin: 0;
    background: #020202;
    color: #f4f4f4;
    font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    letter-spacing: 0;
  }

  button, input, select, textarea {
    font: inherit;
  }

  button {
    min-height: 36px;
    border: 1px solid #6e6e6e;
    border-radius: 4px;
    background: #111;
    color: #fff;
    padding: 0 16px;
    font-weight: 700;
    cursor: pointer;
  }

  button:disabled {
    opacity: 0.35;
    cursor: not-allowed;
  }

  input, select, textarea {
    width: 100%;
    border: 1px solid #3a3a3a;
    border-radius: 4px;
    background: #070707;
    color: #fff;
    padding: 10px 12px;
  }

  textarea {
    min-height: 92px;
    resize: vertical;
  }

  label {
    display: grid;
    gap: 8px;
    color: #b7b7b7;
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
  }

  label small {
    color: #8d8d8d;
    font-size: 11px;
    font-weight: 500;
    line-height: 1.4;
    text-transform: none;
  }

  .auth-shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    background: #000;
  }

  .auth-panel {
    width: min(420px, calc(100vw - 40px));
    border: 1px solid #2c2c2c;
    border-radius: 4px;
    padding: 32px;
    background: #080808;
  }

  .auth-panel .brand {
    text-align: center;
    padding-left: 8px;
  }

  .auth-panel form {
    display: grid;
    gap: 18px;
  }

  .brand {
    letter-spacing: 8px;
    font-size: 24px;
    font-weight: 900;
  }

  .app-shell {
    min-height: 100vh;
    display: grid;
    grid-template-columns: 272px 1fr;
    grid-template-rows: 62px 1fr;
    background: #000;
  }

  .topbar {
    grid-column: 1 / -1;
    border-bottom: 1px solid #1f1f1f;
    display: flex;
    align-items: center;
    gap: 42px;
    padding: 0 64px;
  }

  .topbar nav {
    display: flex;
    gap: 26px;
    flex: 1;
  }

  .topbar nav button, .ghost {
    border-color: transparent;
    background: transparent;
    color: #ddd;
  }

  .topbar nav button.active {
    border-bottom-color: #fff;
    border-radius: 0;
  }

  .sidebar {
    border-right: 1px solid #1f1f1f;
    padding: 24px 16px;
    display: grid;
    align-content: start;
    gap: 8px;
  }

  .sidebar button {
    display: grid;
    grid-template-columns: 36px 1fr;
    align-items: center;
    text-align: left;
    border-color: transparent;
    background: transparent;
    color: #a4a4a4;
  }

  .sidebar button.active {
    background: #2b2b2b;
    color: #fff;
  }

  .content {
    padding: 28px clamp(20px, 5vw, 72px);
    min-width: 0;
  }

  .page-head {
    display: flex;
    align-items: end;
    justify-content: space-between;
    gap: 20px;
    margin-bottom: 24px;
  }

  h1, h2, p {
    margin: 0;
  }

  h1 {
    font-size: 32px;
  }

  .eyebrow, .muted {
    color: #8f8f8f;
    font-size: 12px;
  }

  .stats-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 14px;
    margin-bottom: 18px;
  }

  .stats-grid article, .status-band, .relay-panel, .table-shell, .settings-grid, .modal {
    border: 1px solid #2b2b2b;
    border-radius: 4px;
    background: #050505;
  }

  .stats-grid article {
    padding: 18px;
    display: grid;
    gap: 10px;
  }

  .stats-grid span, .status-band span {
    color: #aaa;
    font-size: 12px;
    text-transform: uppercase;
  }

  .stats-grid strong {
    font-size: 30px;
  }

  .status-band {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  }

  .status-band div {
    padding: 18px;
    border-right: 1px solid #202020;
    display: grid;
    gap: 8px;
  }

  .relay-panel {
    margin-top: 18px;
    overflow: hidden;
  }

  .relay-panel header {
    min-height: 48px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 0 18px;
    border-bottom: 1px solid #1d1d1d;
  }

  .relay-panel header span,
  .relay-state {
    color: #aaa;
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .relay-list {
    display: grid;
  }

  .relay-row {
    min-height: 46px;
    display: grid;
    grid-template-columns: 12px minmax(0, 1fr) auto;
    align-items: center;
    gap: 12px;
    padding: 0 18px;
    border-bottom: 1px solid #171717;
  }

  .relay-row:last-child {
    border-bottom: 0;
  }

  .relay-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: #b44242;
    box-shadow: 0 0 0 3px rgba(180, 66, 66, 0.16);
  }

  .relay-dot.online {
    background: #44d56f;
    box-shadow: 0 0 0 3px rgba(68, 213, 111, 0.16);
  }

  .relay-url {
    min-width: 0;
    overflow: hidden;
    color: #f1f1f1;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .relay-state.online {
    color: #8bf0a5;
  }

  .search {
    display: grid;
    grid-template-columns: minmax(220px, 420px) auto;
    gap: 10px;
  }

  .table-shell {
    overflow: hidden;
  }

  table {
    width: 100%;
    border-collapse: collapse;
  }

  th, td {
    padding: 14px 12px;
    border-bottom: 1px solid #1d1d1d;
    text-align: left;
    vertical-align: middle;
    font-size: 13px;
  }

  th {
    background: #151515;
    color: #d6d6d6;
    font-size: 12px;
    text-transform: uppercase;
  }

  .hash {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }

  .full {
    overflow-wrap: anywhere;
  }

  .events {
    max-width: 280px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .pill {
    display: inline-grid;
    place-items: center;
    min-width: 72px;
    min-height: 24px;
    border: 1px solid #555;
    border-radius: 999px;
    padding: 0 10px;
    color: #ddd;
  }

  .pill.safe { border-color: #2b8a4b; color: #6ee08f; }
  .pill.warn { border-color: #b08b2e; color: #ffd166; }
  .pill.block { border-color: #a33; color: #ff7878; }
  .pill.error { border-color: #8b4bd1; color: #caa8ff; }

  .pager {
    min-height: 54px;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 12px;
    padding: 0 14px;
  }

  .pager select {
    width: 86px;
  }

  .settings-grid {
    padding: 18px;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: 18px;
  }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    width: 100%;
    height: 100%;
    min-height: 0;
    border: 0;
    border-radius: 0;
    padding: 0;
    background: rgba(0, 0, 0, 0.72);
  }

  .modal-wrap {
    position: fixed;
    inset: 0;
    display: grid;
    place-items: center;
    padding: 18px;
    pointer-events: none;
  }

  .modal {
    width: min(680px, 100%);
    padding: 24px;
    display: grid;
    gap: 16px;
    pointer-events: auto;
  }

  .modal a {
    color: #d7d7d7;
    overflow-wrap: anywhere;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 12px;
  }

  .toast-stack {
    position: fixed;
    right: 24px;
    bottom: 24px;
    z-index: 10;
    width: min(380px, calc(100vw - 32px));
    display: grid;
    gap: 10px;
  }

  .toast {
    min-height: 44px;
    width: 100%;
    border-color: #555;
    background: #101010;
    color: #f4f4f4;
    padding: 11px 14px;
    text-align: left;
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.45);
  }

  .toast.success {
    border-color: #2b8a4b;
    color: #8bf0a5;
  }

  .toast.error {
    border-color: #a33;
    color: #ff9b9b;
  }

  .error, .empty {
    color: #ff8585;
  }

  @media (max-width: 760px) {
    .app-shell {
      grid-template-columns: 48px 1fr;
    }

    .topbar {
      padding: 0 18px;
      gap: 18px;
    }

    .topbar nav {
      display: none;
    }

    .brand {
      font-size: 20px;
      letter-spacing: 6px;
    }

    .sidebar {
      padding: 14px 4px;
    }

    .sidebar span {
      display: none;
    }

    .sidebar button {
      grid-template-columns: 1fr;
      padding: 0;
      place-items: center;
    }

    .content {
      padding: 22px 16px;
    }

    .page-head {
      align-items: stretch;
      flex-direction: column;
    }

    .stats-grid {
      grid-template-columns: 1fr 1fr;
    }

    .search {
      grid-template-columns: 1fr;
    }

    .toast-stack {
      right: 16px;
      bottom: 16px;
      width: calc(100vw - 32px);
    }

    th:nth-child(2), td:nth-child(2), th:nth-child(4), td:nth-child(4), th:nth-child(5), td:nth-child(5) {
      display: none;
    }
  }
</style>
