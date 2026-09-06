import { message } from './message';
const app = document.querySelector('#app')!;
app.textContent = message;
if (import.meta.hot) {
  import.meta.hot.accept('./message', (updated) => {
    if (updated) app.textContent = updated.message;
  });
}
