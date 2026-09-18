import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';
import { getSession, logout as apiLogout, setLoginRequiredHandler, type AuthSession } from './api';

export type AuthState =
  | { status: 'loading' }
  /** The server asks for no login (loopback, no `[admin]`). */
  | { status: 'open' }
  | { status: 'signed-in'; user: string; session: AuthSession }
  | { status: 'signed-out'; session: AuthSession | null };

interface AuthContextValue {
  state: AuthState;
  /** Re-reads the session (after a login). */
  refresh: () => Promise<void>;
  signOut: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState>({ status: 'loading' });

  const refresh = useCallback(async () => {
    try {
      const s = await getSession();
      if (!s.required) setState({ status: 'open' });
      else if (s.authenticated && s.user) setState({ status: 'signed-in', user: s.user, session: s });
      else setState({ status: 'signed-out', session: s });
    } catch {
      // Can't tell: let the screens show their own "can't reach the API".
      setState({ status: 'open' });
    }
  }, []);

  useEffect(() => {
    void refresh();
    // Any admin-gate 401 (expired session, logout in another tab) sends
    // the user back to the login screen.
    setLoginRequiredHandler(() => void refresh());
    return () => setLoginRequiredHandler(null);
  }, [refresh]);

  const signOut = useCallback(async () => {
    try {
      await apiLogout();
    } finally {
      await refresh();
    }
  }, [refresh]);

  const value = useMemo(() => ({ state, refresh, signOut }), [state, refresh, signOut]);
  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth outside AuthProvider');
  return ctx;
}
