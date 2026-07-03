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
    media_type: 'image' | 'video';
    sha256: string | null;
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
    provider_response: unknown | null;
    verdict_created_at: string | null;
    job_status: string | null;
    job_error: string | null;
    job_updated_at: string | null;
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

  type Theme = 'light' | 'dark';
  type StreamStatus = 'connecting' | 'live' | 'offline';

  const themeStorageKey = 'aedos-theme';

  const settingHints: Record<string, string> = {
    DEFAULT_POLICY: 'Fallback verdict when an event cannot be fully reviewed. Usually blur_unknown or block_unknown.',
    ENABLE_ESCALATION: 'Reserved for serious incident workflows. Keep false unless you have a legal/process path in place.',
    IMAGE_FETCH_TIMEOUT_SECONDS: 'How long the worker waits when downloading media before marking the job as failed.',
    LABEL_NAMESPACE: 'Nostr label namespace written into published moderation labels.',
    MAX_IMAGE_BYTES: 'Largest image the worker will download and review. Bigger values cost more bandwidth and AI spend.',
    MAX_VIDEO_BYTES: 'Largest video the worker will download before sampling frames for review.',
    MAX_VIDEO_FRAMES: 'Maximum number of video frames sampled and sent to the moderation provider.',
    MODERATION_PROVIDER: 'Media review backend. Use deterministic for local testing or openai for OpenAI moderation.',
    NOSTR_PRIVATE_KEY: 'Secret key used to sign Aedos label events so clients and relays can verify the source.',
    NOSTR_RELAYS: 'Comma-separated relays where Aedos publishes moderation labels.',
    OPENAI_API_KEY: 'OpenAI API key used only when MODERATION_PROVIDER is set to openai.',
    OPENAI_MODERATION_MODEL: 'OpenAI moderation model name used for image review.',
    QUEUE_DEAD_LETTER_MAXLEN: 'Maximum retained failed jobs in the dead-letter stream.',
    QUEUE_STREAM_MAXLEN: 'Approximate maximum retained pending/processed queue entries in Redis.',
    RATE_LIMIT_CHECKS_PER_MINUTE: 'Per-client API limit for moderation check requests.',
    VIDEO_FRAME_INTERVAL_SECONDS: 'Seconds between sampled video frames. Lower values inspect more of each video and cost more.',
    WORKER_CONCURRENCY: 'Number of media jobs the Python worker can process in parallel.'
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
  let showProviderDetails = $state(false);
  let rechecking = $state(false);
  let recheckNotice = $state('');
  let toasts = $state<Toast[]>([]);
  let theme = $state<Theme>('light');
  let mediaStreamStatus = $state<StreamStatus>('offline');
  let nextToastId = 1;
  let mediaRefreshTimer: ReturnType<typeof setTimeout> | null = null;

  const totalPages = $derived(Math.max(1, Math.ceil(images.total / images.per_page)));
  const processedStates = $derived(
    overview ? Object.entries(overview.status_counts).sort(([a], [b]) => a.localeCompare(b)) : []
  );
  const brandLogo = $derived(theme === 'light' ? '/images/logo_small_light.png' : '/images/logo_small_dark.png');
  const hasActiveImageJobs = $derived(images.items.some((item) => isActiveJob(item.job_status)));
  const moderationProvider = $derived(settingValue('MODERATION_PROVIDER'));
  const hasOpenAiKey = $derived(Boolean(settingValue('OPENAI_API_KEY')));
  const openAiReady = $derived(moderationProvider === 'openai' && hasOpenAiKey);
  const openAiStatusTone = $derived(openAiReady ? 'ready' : moderationProvider === 'openai' ? 'error' : 'warn');

  $effect(() => {
    loadTheme();
    void loadSession();
  });

  $effect(() => {
    if (!hasActiveImageJobs || !session.authenticated) return;
    const interval = setInterval(() => {
      void Promise.all([loadOverview(), loadImages()]).then(() => {
        if (selected) refreshSelectedFromImages(selected);
      });
    }, 2500);
    return () => clearInterval(interval);
  });

  $effect(() => {
    if (!session.authenticated) {
      mediaStreamStatus = 'offline';
      return;
    }
    if (typeof window === 'undefined') return;

    let closed = false;
    let socket: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    const connect = () => {
      mediaStreamStatus = 'connecting';
      socket = new WebSocket(adminStreamUrl());
      socket.onopen = () => {
        mediaStreamStatus = 'live';
      };
      socket.onmessage = (event) => {
        const message = parseStreamMessage(event.data);
        if (message?.type === 'media_changed') {
          scheduleMediaRefresh();
        }
      };
      socket.onerror = () => {
        socket?.close();
      };
      socket.onclose = () => {
        if (closed) return;
        mediaStreamStatus = 'offline';
        reconnectTimer = setTimeout(connect, 3000);
      };
    };

    connect();

    return () => {
      closed = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      socket?.close();
      mediaStreamStatus = 'offline';
    };
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

  function scheduleMediaRefresh() {
    if (mediaRefreshTimer) clearTimeout(mediaRefreshTimer);
    mediaRefreshTimer = setTimeout(() => {
      mediaRefreshTimer = null;
      void Promise.all([loadOverview(), loadImages()]).then(() => {
        if (selected) refreshSelectedFromImages(selected);
      });
    }, 200);
  }

  function adminStreamUrl() {
    const url = new URL('/admin/api/stream', window.location.href);
    url.protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    if (url.port === '3000') {
      url.port = '8080';
    }
    return url.toString();
  }

  function parseStreamMessage(data: unknown): { type?: string } | null {
    if (typeof data !== 'string') return null;
    try {
      return JSON.parse(data);
    } catch {
      return null;
    }
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
    showProviderDetails = false;
    recheckNotice = '';
  }

  async function saveReview() {
    if (!selected) return;
    if (!selected.sha256) {
      notify(`This URL has not been fetched and hashed yet, so there is no ${mediaName(selected)} verdict to edit.`, 'error');
      return;
    }
    try {
      await api(`/admin/api/${mediaRoute(selected)}/${selected.sha256}/verdict`, {
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

  async function recheckImage() {
    if (!selected || rechecking) return;
    if (!selected.sha256) {
      notify(`This URL has not been fetched yet. Try another ${mediaName(selected)} URL or wait for the fetch error to clear.`, 'error');
      return;
    }
    const sha256 = selected.sha256;
    rechecking = true;
    recheckNotice = 'Queued. Waiting for the worker to write a fresh verdict.';
    try {
      await api(`/admin/api/${mediaRoute(selected)}/${sha256}/recheck`, {
        method: 'POST',
        body: '{}'
      });
      notify(`${mediaTitle(selected)} recheck queued`, 'success');
      await Promise.all([loadOverview(), loadImages()]);
      refreshSelectedFromImages(selected);
    } catch (error) {
      notify(error instanceof Error ? error.message : `Could not queue ${mediaName(selected)} recheck`, 'error');
      recheckNotice = '';
    } finally {
      rechecking = false;
    }
  }

  function refreshSelectedFromImages(item: ImageItem) {
    const updated = images.items.find((candidate) =>
      item.sha256 ? candidate.sha256 === item.sha256 : candidate.url === item.url && candidate.event_ids[0] === item.event_ids[0]
    );
    if (!updated) return;
    selected = updated;
    reviewStatus = updated.status ?? 'safe';
    reviewLabels = updated.labels.length ? updated.labels.join(', ') : reviewStatus;
    reviewConfidence = updated.confidence ?? 1;
    reviewExplanation = updated.explanation ?? '';
    showProviderDetails = false;
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

  function reviewSourceText(item: ImageItem) {
    if (!item.sha256 && item.job_status === 'failed') return `${mediaTitle(item)} fetch failed before AI review`;
    if (!item.sha256) return `Waiting for ${mediaName(item)} fetch`;
    if (item.source === 'openai_moderation') return `OpenAI reviewed this ${mediaName(item)}`;
    if (item.source === 'local_model') return 'Local development model, not OpenAI';
    return item.source ? `Reviewed by ${item.source}` : 'No reviewer recorded yet';
  }

  function mediaName(item: ImageItem) {
    return item.media_type === 'video' ? 'video' : 'image';
  }

  function mediaTitle(item: ImageItem) {
    return item.media_type === 'video' ? 'Video' : 'Image';
  }

  function mediaRoute(item: ImageItem) {
    return item.media_type === 'video' ? 'videos' : 'images';
  }

  function isActiveJob(status: string | null) {
    return status === 'queued' || status === 'processing' || status === 'retrying';
  }

  function jobLabel(status: string | null) {
    if (status === 'queued') return 'Queued';
    if (status === 'processing') return 'Processing';
    if (status === 'retrying') return 'Retrying';
    if (status === 'failed') return 'Failed';
    return '';
  }

  function settingValue(key: string) {
    return settings.find((setting) => setting.key === key)?.value.trim() ?? '';
  }

  function openAiStatusTitle() {
    if (openAiReady) return 'OpenAI connected and ready';
    if (moderationProvider === 'openai') return 'OpenAI selected, API key missing';
    if (hasOpenAiKey) return 'OpenAI key saved, but not active';
    return 'OpenAI not connected';
  }

  function openAiStatusBody() {
    if (openAiReady) return 'Recheck with AI will use OpenAI moderation for new media reviews.';
    if (moderationProvider === 'openai') return 'Add OPENAI_API_KEY, save settings, then recheck the media item.';
    if (hasOpenAiKey) return 'Change MODERATION_PROVIDER from deterministic to openai, save settings, then recheck the media item.';
    return 'Add OPENAI_API_KEY, set MODERATION_PROVIDER to openai, save settings, then recheck the media item.';
  }

  function providerResponseText(value: unknown | null) {
    if (!value) return '';
    return JSON.stringify(value, null, 2);
  }
</script>

<svelte:head>
  <title>Aedos Control</title>
</svelte:head>

{#if loading}
  <main class="auth-shell">
    <button class="theme-toggle auth-theme-toggle" type="button" onclick={toggleTheme}>{theme === 'light' ? 'Dark' : 'Light'}</button>
    <div class="brand"><img src={brandLogo} alt="" />AEDOS</div>
    <p>Loading control surface</p>
  </main>
{:else if !session.authenticated}
  <main class="auth-shell">
    <button class="theme-toggle auth-theme-toggle" type="button" onclick={toggleTheme}>{theme === 'light' ? 'Dark' : 'Light'}</button>
    <section class="auth-panel">
      <div class="brand"><img src={brandLogo} alt="" />AEDOS</div>
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
      <div class="brand"><img src={brandLogo} alt="" />AEDOS</div>
      <nav>
        <button class:active={activeView === 'dashboard'} onclick={() => (activeView = 'dashboard')}>Overview</button>
        <button class:active={activeView === 'images'} onclick={() => (activeView = 'images')}>Media</button>
        <button class:active={activeView === 'settings'} onclick={() => (activeView = 'settings')}>Settings</button>
      </nav>
      <button class="theme-toggle" type="button" onclick={toggleTheme}>{theme === 'light' ? 'Dark' : 'Light'}</button>
      <button class="ghost" onclick={logout}>{session.username}</button>
    </header>

    <aside class="sidebar">
      <button class:active={activeView === 'dashboard'} onclick={() => (activeView = 'dashboard')}>⌂<span>Overview</span></button>
      <button class:active={activeView === 'images'} onclick={() => (activeView = 'images')}>▦<span>Media</span></button>
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
            <div class="title-row">
              <h1>Media</h1>
              <span class={`live-indicator ${mediaStreamStatus}`}>
                <span></span>
                {mediaStreamStatus === 'live' ? 'Live' : mediaStreamStatus === 'connecting' ? 'Connecting' : 'Reconnecting'}
              </span>
            </div>
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
                <th>Media</th>
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
                    <div class="media-type">{mediaTitle(item)}</div>
                    <div class="hash">{item.sha256 ? short(item.sha256) : 'not fetched'}</div>
                    <div class="muted">{item.mime_type ?? 'unknown'} · {formatBytes(item.bytes)}</div>
                  </td>
                  <td>
                    <div class="events">{item.event_ids.slice(0, 2).join(', ') || '-'}</div>
                  </td>
                  <td>
                    <div class="status-cell">
                      <span class={`pill ${item.status ?? 'unknown'}`}>{item.status ?? 'unknown'}</span>
                      {#if isActiveJob(item.job_status)}
                        <button class="job-detail" type="button" title={item.job_error ?? `${jobLabel(item.job_status)} ${mediaName(item)} review`} onclick={() => openReview(item)}>
                          <span class="job-progress" aria-label={jobLabel(item.job_status)}>
                            <span></span>
                          </span>
                          <small>{item.job_error ? `${jobLabel(item.job_status)}: ${item.job_error}` : jobLabel(item.job_status)}</small>
                        </button>
                      {:else if item.job_status === 'failed'}
                        <button class="job-detail job-error" type="button" title={item.job_error ?? 'processing failed'} onclick={() => openReview(item)}>
                          <small>{item.job_error ?? 'Failed'}</small>
                        </button>
                      {/if}
                    </div>
                  </td>
                  <td>{item.source ?? '-'}</td>
                  <td>{new Date(item.first_seen_at).toLocaleString()}</td>
                  <td><button onclick={() => openReview(item)}>{item.sha256 ? 'Review' : 'Details'}</button></td>
                </tr>
              {/each}
              {#if images.items.length === 0}
                <tr><td colspan="6" class="empty">No media yet</td></tr>
              {/if}
            </tbody>
          </table>
          <footer class="pager">
            <span>{images.total} media items</span>
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

        <section class={`provider-status ${openAiStatusTone}`}>
          <div>
            <span>AI Provider</span>
            <strong>{openAiStatusTitle()}</strong>
            <p>{openAiStatusBody()}</p>
            {#each settings as setting}
              {#if setting.key === 'MODERATION_PROVIDER'}
                <label class="provider-select">
                  Active reviewer
                  <select bind:value={setting.value}>
                    <option value="deterministic">Local test model</option>
                    <option value="openai">OpenAI moderation</option>
                  </select>
                  <small>Choose OpenAI moderation here, then save settings before rechecking media.</small>
                </label>
              {/if}
            {/each}
          </div>
          <dl>
            <div>
              <dt>Provider</dt>
              <dd>{moderationProvider || 'deterministic'}</dd>
            </div>
            <div>
              <dt>OpenAI Key</dt>
              <dd>{hasOpenAiKey ? 'saved' : 'missing'}</dd>
            </div>
            <div>
              <dt>Model</dt>
              <dd>{settingValue('OPENAI_MODERATION_MODEL') || '-'}</dd>
            </div>
          </dl>
        </section>

        <section class="settings-grid">
          {#each settings as setting}
            {#if setting.key !== 'MODERATION_PROVIDER'}
              <label>
                <span>{setting.key}</span>
                <input bind:value={setting.value} type={setting.secret ? 'password' : 'text'} autocomplete="off" />
                {#if settingHints[setting.key]}
                  <small>{settingHints[setting.key]}</small>
                {/if}
              </label>
            {/if}
          {/each}
        </section>
      {/if}
    </section>

    {#if selected}
      <button class="modal-backdrop" type="button" aria-label="Close review" onclick={() => (selected = null)}></button>
      <section class="modal-wrap" aria-modal="true" role="dialog">
        <form class="modal" onsubmit={(event) => { event.preventDefault(); void saveReview(); }}>
          <h2>{selected.sha256 ? `Review ${mediaTitle(selected)}` : `${mediaTitle(selected)} Job Details`}</h2>
          <p class="hash full">{selected.sha256 ?? `${mediaTitle(selected)} was not fetched and hashed`}</p>
          <a href={selected.url} target="_blank" rel="noreferrer">{selected.url}</a>
          <section class="review-evidence">
            <div class:selected-ok={selected.source === 'openai_moderation'}>
              <span>Reviewer</span>
              <strong>{reviewSourceText(selected)}</strong>
            </div>
            <div>
              <span>Source</span>
              <strong>{selected.source ?? '-'}</strong>
            </div>
            <div>
              <span>Model</span>
              <strong>{selected.model_version ?? '-'}</strong>
            </div>
            <div>
              <span>Last Verdict</span>
              <strong>{selected.verdict_created_at ? new Date(selected.verdict_created_at).toLocaleString() : '-'}</strong>
            </div>
          </section>
          {#if selected.provider_response}
            <section class="provider-details">
              <button class="ghost" type="button" onclick={() => (showProviderDetails = !showProviderDetails)}>
                {showProviderDetails ? 'Hide AI Details' : 'Show AI Details'}
              </button>
              {#if showProviderDetails}
                <pre>{providerResponseText(selected.provider_response)}</pre>
              {/if}
            </section>
          {/if}
          {#if recheckNotice}
            <p class="recheck-notice">{recheckNotice}</p>
          {/if}
          {#if selected.job_status || selected.job_error}
            <section class="job-summary">
              <span>Job Status</span>
              <strong>{jobLabel(selected.job_status) || selected.job_status || 'Completed'}</strong>
              {#if selected.job_error}
                <p>{selected.job_error}</p>
              {/if}
            </section>
          {/if}
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
            <button type="button" class="ghost" disabled={rechecking || !selected.sha256} onclick={recheckImage}>{rechecking ? 'Queueing...' : 'Recheck with AI'}</button>
            <button type="button" class="ghost" onclick={() => (selected = null)}>Cancel</button>
            <button type="submit" disabled={!selected.sha256}>Save Verdict</button>
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

  :global(:root) {
    --bg: #fffdf8;
    --surface: #fffdf8;
    --surface-active: #ece9e1;
    --table-head: #ece9e1;
    --text: #000;
    --muted: #55545c;
    --line: #55545c;
    --danger: #8f2424;
    --modal-backdrop: rgba(0, 0, 0, 0.38);
    --toast-shadow: rgba(0, 0, 0, 0.18);
  }

  :global(:root[data-theme='dark']) {
    --bg: #000;
    --surface: #050505;
    --surface-active: #2a2a2a;
    --table-head: #111;
    --text: #f5f5f5;
    --muted: #a5a5a5;
    --line: #55545c;
    --danger: #ff8585;
    --modal-backdrop: rgba(0, 0, 0, 0.72);
    --toast-shadow: rgba(0, 0, 0, 0.45);
  }

  :global(body) {
    margin: 0;
    background: var(--bg);
    color: var(--text);
    font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    letter-spacing: 0;
  }

  button, input, select, textarea {
    font: inherit;
  }

  button {
    min-height: 36px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
    color: var(--text);
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
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
    color: var(--text);
    padding: 10px 12px;
  }

  textarea {
    min-height: 92px;
    resize: vertical;
  }

  label {
    display: grid;
    gap: 8px;
    color: var(--text);
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
  }

  label small {
    color: var(--muted);
    font-size: 11px;
    font-weight: 500;
    line-height: 1.4;
    text-transform: none;
  }

  .auth-shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    background: var(--bg);
  }

  .auth-panel {
    width: min(420px, calc(100vw - 40px));
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 32px;
    background: var(--surface);
  }

  .auth-panel .brand {
    justify-content: center;
    text-align: center;
  }

  .auth-panel form {
    display: grid;
    gap: 18px;
  }

  .brand {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    letter-spacing: 8px;
    font-size: 24px;
    font-weight: 900;
  }

  .brand img {
    width: 30px;
    height: 30px;
    flex: 0 0 auto;
    object-fit: contain;
  }

  .theme-toggle {
    min-width: 74px;
  }

  .auth-theme-toggle {
    position: fixed;
    top: 18px;
    right: 18px;
  }

  .app-shell {
    min-height: 100vh;
    display: grid;
    grid-template-columns: 272px 1fr;
    grid-template-rows: 62px 1fr;
    background: var(--bg);
  }

  .topbar {
    grid-column: 1 / -1;
    border-bottom: 1px solid var(--line);
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
    color: var(--text);
  }

  .topbar nav button.active {
    border-bottom-color: var(--line);
    border-radius: 0;
  }

  .sidebar {
    border-right: 1px solid var(--line);
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
    color: var(--text);
  }

  .sidebar button.active {
    background: var(--surface-active);
    color: var(--text);
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
    color: var(--muted);
    font-size: 12px;
  }

  .title-row {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-wrap: wrap;
  }

  .live-indicator {
    min-height: 24px;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 0 10px;
    border: 1px solid var(--line);
    border-radius: 999px;
    color: var(--muted);
    font-size: 11px;
    font-weight: 800;
    text-transform: uppercase;
  }

  .live-indicator span {
    width: 7px;
    height: 7px;
    border-radius: 999px;
    background: #a33f3f;
    box-shadow: 0 0 0 3px color-mix(in srgb, #a33f3f 18%, transparent);
  }

  .live-indicator.live {
    color: #21c566;
  }

  .live-indicator.live span {
    background: #21c566;
    box-shadow: 0 0 0 3px color-mix(in srgb, #21c566 18%, transparent);
  }

  .live-indicator.connecting span {
    background: #c49a25;
    box-shadow: 0 0 0 3px color-mix(in srgb, #c49a25 18%, transparent);
  }

  .stats-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 14px;
    margin-bottom: 18px;
  }

  .stats-grid article, .status-band, .relay-panel, .table-shell, .settings-grid, .modal {
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
  }

  .stats-grid article {
    padding: 18px;
    display: grid;
    gap: 10px;
  }

  .stats-grid span, .status-band span {
    color: var(--muted);
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
    border-right: 1px solid var(--line);
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
    border-bottom: 1px solid var(--line);
  }

  .relay-panel header span,
  .relay-state {
    color: var(--muted);
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
    border-bottom: 1px solid var(--line);
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
    color: var(--text);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .relay-state.online {
    color: var(--text);
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
    border-bottom: 1px solid var(--line);
    text-align: left;
    vertical-align: middle;
    font-size: 13px;
  }

  th {
    background: var(--table-head);
    color: var(--text);
    font-size: 12px;
    text-transform: uppercase;
  }

  .hash {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }

  .media-type {
    color: var(--muted);
    font-size: 11px;
    font-weight: 800;
    text-transform: uppercase;
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
    border: 1px solid var(--line);
    border-radius: 999px;
    padding: 0 10px;
    color: var(--text);
  }

  .pill.safe, .pill.warn, .pill.block, .pill.error {
    border-color: var(--line);
    color: var(--text);
  }

  .status-cell {
    min-width: 120px;
    display: grid;
    gap: 6px;
  }

  .job-detail {
    width: min(220px, 100%);
    min-height: 0;
    border: 0;
    padding: 0;
    display: grid;
    gap: 5px;
    background: transparent;
    color: var(--muted);
    text-align: left;
  }

  .status-cell small {
    max-width: 180px;
    overflow: hidden;
    color: var(--muted);
    font-size: 11px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .job-progress {
    position: relative;
    width: 96px;
    height: 3px;
    overflow: hidden;
    border: 1px solid var(--line);
    border-radius: 999px;
    background: var(--surface-active);
  }

  .job-progress span {
    position: absolute;
    inset: -1px auto -1px -36px;
    width: 36px;
    border-radius: inherit;
    background: var(--text);
    animation: progress-slide 1.1s linear infinite;
  }

  .job-error,
  .job-error small {
    color: var(--danger);
  }

  .job-summary {
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 10px 12px;
    display: grid;
    gap: 6px;
    font-size: 12px;
  }

  .job-summary span {
    color: var(--muted);
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .job-summary p {
    color: var(--danger);
    overflow-wrap: anywhere;
  }

  @keyframes progress-slide {
    to {
      transform: translateX(132px);
    }
  }

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

  .provider-status {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: stretch;
    gap: 18px;
    border: 1px solid var(--line);
    border-radius: 4px;
    margin-bottom: 18px;
    padding: 16px;
    background: var(--surface);
  }

  .provider-status span,
  .provider-status dt {
    color: var(--muted);
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .provider-status strong {
    display: block;
    margin-top: 6px;
    font-size: 18px;
  }

  .provider-status p {
    margin-top: 8px;
    color: var(--muted);
    font-size: 13px;
  }

  .provider-select {
    margin-top: 18px;
    max-width: 420px;
  }

  .provider-select select {
    min-height: 44px;
    font-weight: 800;
  }

  .provider-status dl {
    min-width: min(520px, 40vw);
    margin: 0;
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    border: 1px solid var(--line);
    border-radius: 4px;
    overflow: hidden;
  }

  .provider-status dl div {
    min-width: 0;
    padding: 10px 12px;
    border-right: 1px solid var(--line);
  }

  .provider-status dl div:last-child {
    border-right: 0;
  }

  .provider-status dd {
    margin: 6px 0 0;
    overflow: hidden;
    font-weight: 700;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .provider-status.ready {
    border-color: #1f9d4e;
  }

  .provider-status.ready strong {
    color: #1f9d4e;
  }

  .provider-status.warn,
  .provider-status.error {
    border-color: var(--line);
  }

  .provider-status.warn strong,
  .provider-status.error strong {
    color: var(--danger);
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
    background: var(--modal-backdrop);
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
    color: var(--text);
    overflow-wrap: anywhere;
  }

  .review-evidence {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    border: 1px solid var(--line);
    border-radius: 4px;
    overflow: hidden;
  }

  .review-evidence div {
    min-width: 0;
    padding: 12px;
    border-right: 1px solid var(--line);
    border-bottom: 1px solid var(--line);
    display: grid;
    gap: 6px;
  }

  .review-evidence div:nth-child(2n) {
    border-right: 0;
  }

  .review-evidence div:nth-last-child(-n + 2) {
    border-bottom: 0;
  }

  .review-evidence span {
    color: var(--muted);
    font-size: 11px;
    font-weight: 700;
    text-transform: uppercase;
  }

  .review-evidence strong {
    min-width: 0;
    overflow: hidden;
    font-size: 13px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .review-evidence .selected-ok strong {
    color: #1f9d4e;
  }

  .provider-details {
    display: grid;
    gap: 10px;
    justify-items: start;
  }

  .provider-details pre {
    width: 100%;
    max-height: 260px;
    margin: 0;
    overflow: auto;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
    color: var(--text);
    padding: 12px;
    font-size: 12px;
    line-height: 1.5;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .recheck-notice {
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 10px 12px;
    color: var(--muted);
    font-size: 12px;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 12px;
  }

  .toast-stack {
    position: fixed;
    bottom: 24px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 10;
    width: min(420px, calc(100vw - 32px));
    display: grid;
    gap: 10px;
  }

  .toast {
    min-height: 44px;
    width: 100%;
    border-color: var(--line);
    background: var(--surface);
    color: var(--text);
    padding: 11px 14px;
    text-align: center;
    box-shadow: 0 12px 28px var(--toast-shadow);
  }

  .toast.success {
    border-color: var(--line);
    color: var(--text);
  }

  .toast.error {
    border-color: var(--line);
    color: var(--text);
  }

  .error, .empty {
    color: var(--danger);
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

    .provider-status {
      grid-template-columns: 1fr;
    }

    .provider-status dl {
      min-width: 0;
      grid-template-columns: 1fr;
    }

    .provider-status dl div {
      border-right: 0;
      border-bottom: 1px solid var(--line);
    }

    .provider-status dl div:last-child {
      border-bottom: 0;
    }

    .review-evidence {
      grid-template-columns: 1fr;
    }

    .review-evidence div,
    .review-evidence div:nth-child(2n) {
      border-right: 0;
      border-bottom: 1px solid var(--line);
    }

    .review-evidence div:last-child {
      border-bottom: 0;
    }

    .toast-stack {
      bottom: 16px;
      width: calc(100vw - 32px);
    }

    th:nth-child(2), td:nth-child(2), th:nth-child(4), td:nth-child(4), th:nth-child(5), td:nth-child(5) {
      display: none;
    }
  }
</style>
