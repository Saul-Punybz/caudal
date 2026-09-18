import { Route, Routes } from 'react-router-dom';
import { NavRail } from './components/NavRail';
import { Overview } from './pages/Overview';
import { StreamDetail } from './pages/StreamDetail';
import { Publish } from './pages/Publish';

export function App() {
  return (
    <div className="flex h-screen w-screen overflow-hidden bg-surface text-on-surface">
      <NavRail />
      <Routes>
        <Route path="/" element={<Overview />} />
        <Route path="/publish" element={<Publish />} />
        <Route path="/streams/:name" element={<StreamDetail />} />
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
