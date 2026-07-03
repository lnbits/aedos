<script lang="ts">
  import { goto } from '$app/navigation';

  type Session = {
    authenticated: boolean;
    username: string | null;
    needs_setup: boolean;
  };

  type Toast = {
    id: number;
    message: string;
    tone: 'success' | 'error' | 'info';
  };

  type Theme = 'light' | 'dark';

  const themeStorageKey = 'aedos-theme';

  let session = $state<Session>({ authenticated: false, username: null, needs_setup: true });
  let username = $state('');
  let password = $state('');
  let confirmPassword = $state('');
  let loading = $state(true);
  let theme = $state<Theme>('light');
  let toasts = $state<Toast[]>([]);
  let nextToastId = 1;

  const brandLogo = $derived(theme === 'light' ? '/images/logo_small_light.png' : '/images/logo_small_dark.png');

  $effect(() => {
    loadTheme();
    void loadSession();
  });

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
        await goto('/admin');
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
      await goto('/admin');
    } catch (error) {
      notify(error instanceof Error ? error.message : 'Authentication failed', 'error');
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
</script>

<svelte:head>
  <title>Aedos Login</title>
</svelte:head>

<main class="auth-shell">
  <a class="home-link" href="/">AEDOS</a>
  <button class="theme-toggle" type="button" onclick={toggleTheme}>{theme === 'light' ? 'Dark' : 'Light'}</button>

  {#if loading}
    <section class="auth-panel">
      <div class="brand"><img src={brandLogo} alt="" />AEDOS</div>
      <p>Loading control surface</p>
    </section>
  {:else}
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
  {/if}

  <section class="toasts" aria-live="polite">
    {#each toasts as toast}
      <button class={`toast ${toast.tone}`} type="button" onclick={() => dismissToast(toast.id)}>{toast.message}</button>
    {/each}
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
    --danger: #c9252d;
    --success: #008c45;
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
  }

  button, input {
    font: inherit;
  }

  .auth-shell {
    min-height: 100vh;
    display: grid;
    place-items: center;
    padding: 28px;
  }

  .home-link {
    position: fixed;
    top: 22px;
    left: 28px;
    color: var(--text);
    text-decoration: none;
    font-weight: 900;
    letter-spacing: 0.22em;
  }

  .theme-toggle {
    position: fixed;
    top: 16px;
    right: 28px;
  }

  .auth-panel {
    width: min(420px, 100%);
    border: 1px solid var(--line);
    padding: 32px;
    background: var(--surface);
  }

  .brand {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 13px;
    font-size: 1.5rem;
    font-weight: 900;
    letter-spacing: 0.26em;
  }

  .brand img {
    width: 38px;
    height: 38px;
    object-fit: contain;
  }

  h1 {
    margin: 22px 0 10px;
    font-size: 2rem;
    letter-spacing: 0;
  }

  form,
  label {
    display: grid;
    gap: 12px;
  }

  form {
    margin-top: 18px;
    gap: 18px;
  }

  label {
    color: var(--muted);
    font-size: 0.78rem;
    font-weight: 900;
    text-transform: uppercase;
  }

  input {
    width: 100%;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--bg);
    color: var(--text);
    padding: 12px;
  }

  button {
    min-height: 38px;
    border: 1px solid var(--line);
    border-radius: 4px;
    background: var(--surface);
    color: var(--text);
    padding: 0 16px;
    font-weight: 800;
    cursor: pointer;
  }

  .toasts {
    position: fixed;
    left: 50%;
    bottom: 22px;
    transform: translateX(-50%);
    display: grid;
    gap: 8px;
    width: min(440px, calc(100vw - 28px));
  }

  .toast {
    width: 100%;
    min-height: 40px;
    text-align: left;
    background: var(--surface);
  }

  .toast.error {
    border-color: var(--danger);
    color: var(--danger);
  }

  .toast.success {
    border-color: var(--success);
    color: var(--success);
  }
</style>
