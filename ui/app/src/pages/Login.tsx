import { useState } from 'react';
import type { FormEvent } from 'react';
import { ApiError, SSO_START_URL, login, type AuthSession } from '../api';
import { ThemeToggle } from '../components/ThemeToggle';

interface Props {
  session: AuthSession | null;
  onSignedIn: () => Promise<void>;
}

/** What `/api/v1/auth/oidc/callback` redirects here with on failure. */
function ssoMessage(code: string | null): string | null {
  if (code === 'sso') return 'Single sign-on refused. Your account may not be allowed on this server.';
  if (code === 'sso_unavailable') return "Can't reach the sign-in provider. Try again, or use a password.";
  return null;
}

/** Full-screen sign-in: name + password and/or "Sign in with SSO",
 * whichever `[admin]` configures. */
export function Login({ session, onSignedIn }: Props) {
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(() =>
    ssoMessage(new URLSearchParams(window.location.search).get('error')),
  );

  const showPassword = session?.password ?? true;
  const showSso = session?.oidc ?? false;

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(name, password);
      setPassword('');
      if (window.location.pathname === '/login') window.history.replaceState(null, '', '/');
      await onSignedIn();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) setError('Wrong name or password.');
      else if (err instanceof ApiError && err.status === 429)
        setError(`Too many attempts. Try again in ${err.retryAfter ?? 60} s.`);
      else setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="relative flex h-screen w-screen items-center justify-center overflow-y-auto bg-surface p-6 text-on-surface">
      <div className="absolute right-4 top-4">
        <ThemeToggle />
      </div>
      <section
        aria-labelledby="login-title"
        className="flex w-full max-w-sm flex-col gap-6 rounded-xl bg-surface-container-low p-8"
      >
        <header className="flex flex-col items-center gap-4 text-center">
          <div
            aria-hidden="true"
            className="flex h-14 w-14 items-center justify-center rounded-2xl bg-primary-container text-on-primary-container"
          >
            <svg width="32" height="32" viewBox="0 0 28 28" fill="none">
              <path
                d="M4 10c4-4 8-4 10 0s6 4 10 0M4 18c4-4 8-4 10 0s6 4 10 0"
                stroke="currentColor"
                strokeWidth="3"
                strokeLinecap="round"
              />
            </svg>
          </div>
          <div>
            <h1 id="login-title" className="display m-0 text-3xl leading-none">
              Sign in
            </h1>
            <p className="mt-2 text-sm text-on-surface-variant">This Caudal server needs an admin login.</p>
          </div>
        </header>

        {error && (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-md bg-error-container px-4 py-3 text-sm text-on-error-container"
          >
            <span className="ms text-lg" aria-hidden="true">
              error
            </span>
            {error}
          </div>
        )}

        {showPassword && (
          <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-4">
            <label className="flex flex-col gap-1.5 text-sm font-medium text-on-surface-variant">
              Name
              <input
                type="text"
                name="username"
                autoComplete="username"
                required
                autoFocus
                value={name}
                onChange={(e) => setName(e.target.value)}
                className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-base text-on-surface outline-none focus:border-primary"
              />
            </label>
            <label className="flex flex-col gap-1.5 text-sm font-medium text-on-surface-variant">
              Password
              <input
                type="password"
                name="password"
                autoComplete="current-password"
                required
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-base text-on-surface outline-none focus:border-primary"
              />
            </label>
            <button
              type="submit"
              disabled={busy}
              className="state-layer mt-2 flex h-11 items-center justify-center gap-2 rounded-full border-0 bg-primary px-6 text-sm font-semibold text-on-primary disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy && (
                <span className="ms animate-spin text-lg" aria-hidden="true">
                  sync
                </span>
              )}
              Sign in
            </button>
          </form>
        )}

        {showPassword && showSso && (
          <div className="flex items-center gap-3 text-xs text-on-surface-variant" aria-hidden="true">
            <span className="h-px flex-grow bg-outline-variant" />
            or
            <span className="h-px flex-grow bg-outline-variant" />
          </div>
        )}

        {showSso && (
          <a
            href={SSO_START_URL}
            className="state-layer flex h-11 items-center justify-center gap-2 rounded-full bg-secondary-container px-6 text-sm font-semibold text-on-secondary-container no-underline"
          >
            <span className="ms text-lg" aria-hidden="true">
              badge
            </span>
            Sign in with SSO
          </a>
        )}
      </section>
    </main>
  );
}
