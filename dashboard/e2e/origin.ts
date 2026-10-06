// Where `vite preview` serves the built app for the end-to-end tests: the
// port vite.config.ts gives it, on the loopback address only.

export const APP_ORIGIN = "http://127.0.0.1:4173";

/** The app's own root, as open-ferry serves it. */
export const APP_URL = `${APP_ORIGIN}/dashboard/`;
