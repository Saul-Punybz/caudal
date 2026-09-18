import { Navigate, Route, Routes, useLocation } from 'react-router-dom';
import { NavRail } from './components/NavRail';
import { Overview } from './pages/Overview';
import { StreamDetail } from './pages/StreamDetail';
import { Publish } from './pages/Publish';
import { Channels } from './pages/Channels';
import { Restreams } from './pages/Restreams';
import { Recordings } from './pages/Recordings';
import { Login } from './pages/Login';
import { useAuth } from './auth';

export function App() {
  const { state, refresh } = useAuth();
  const location = useLocation();

  if (state.status === 'loading') {
    return <div className="h-screen w-screen bg-surface" aria-busy="true" />;
  }
  if (state.status === 'signed-out') {
    return <Login session={state.session} onSignedIn={refresh} />;
  }
  // Signed in (or no login needed): /login has nothing left to do.
  if (location.pathname === '/login') {
    return <Navigate to="/" replace />;
  }

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-surface text-on-surface">
      <NavRail />
      <Routes>
        <Route path="/" element={<Overview />} />
        <Route path="/publish" element={<Publish />} />
        <Route path="/streams/:name" element={<StreamDetail />} />
        <Route path="/channels" element={<Channels />} />
        <Route path="/restreams" element={<Restreams />} />
        <Route path="/recordings" element={<Recordings />} />
        <Route
          path="*"
          element={
            <main className="flex flex-grow items-center justify-center text-on-surface-variant">
              Not found.
            </main>
          }
        />
      </Routes>
    </div>
  );
}
