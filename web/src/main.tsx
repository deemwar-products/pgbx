import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { initToken } from './api';
import { App } from './App';
import './styles.css';

const hasToken = initToken() !== '';
// a new run's link opened in this same tab: take its token and start over
window.addEventListener('hashchange', () => {
  if (/(?:^#|&)token=/.test(location.hash)) {
    initToken();
    location.reload();
  }
});
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App hasToken={hasToken} />
  </StrictMode>,
);
