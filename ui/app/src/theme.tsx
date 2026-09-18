import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import type { ReactNode } from 'react';

type ThemeMode = 'dark' | 'light';
const STORAGE_KEY = 'caudal-theme';

function readStored(): ThemeMode | null {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    return v === 'dark' || v === 'light' ? v : null;
  } catch {
    // localStorage can throw (private mode, disabled storage); fall back
    // to system preference only.
    return null;
  }
}

function writeStored(mode: ThemeMode) {
  try {
    localStorage.setItem(STORAGE_KEY, mode);
  } catch {
    // Best-effort only.
  }
}

interface ThemeContextValue {
  /** The explicit choice, or null to follow prefers-color-scheme. */
  mode: ThemeMode | null;
  /** The mode actually in effect right now, for rendering the toggle icon. */
  effective: ThemeMode;
  toggle: () => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [mode, setMode] = useState<ThemeMode | null>(() => readStored());
  const [systemPrefersDark, setSystemPrefersDark] = useState(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return true;
    return window.matchMedia('(prefers-color-scheme: dark)').matches;
  });

  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return;
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = (e: MediaQueryListEvent) => setSystemPrefersDark(e.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);

  const effective: ThemeMode = mode ?? (systemPrefersDark ? 'dark' : 'light');

  useEffect(() => {
    const root = document.documentElement;
    if (mode) {
      root.setAttribute('data-theme', mode);
    } else {
      root.removeAttribute('data-theme');
    }
  }, [mode]);

  const toggle = useCallback(() => {
    setMode((_prev) => {
      const next: ThemeMode = effective === 'dark' ? 'light' : 'dark';
      writeStored(next);
      return next;
    });
  }, [effective]);

  const value = useMemo(() => ({ mode, effective, toggle }), [mode, effective, toggle]);

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}

export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error('useTheme must be used within a ThemeProvider');
  return ctx;
}
