/** Accept only local paths for navigation after sign-in. */
export function safeNextPath(value: string | null): string {
  if (!value || !value.startsWith("/") || value.startsWith("//"))
    return "/overview";
  try {
    const path = value.split(/[?#]/u, 1)[0];
    const decoded = decodeURIComponent(path);
    if (/[\\\u0000-\u001f\u007f]/u.test(decoded) || decoded.startsWith("//"))
      return "/overview";
    const url = new URL(value, "http://local.invalid");
    // Dot segments can normalize a single-root-relative input to `//host`;
    // returning that string to the router would turn it into an external URL.
    const normalizedPath = decodeURIComponent(url.pathname);
    if (url.origin !== "http://local.invalid" || normalizedPath.startsWith("//") ||
      /[\\\u0000-\u001f\u007f]/u.test(normalizedPath))
      return "/overview";
    return `${url.pathname}${url.search}${url.hash}`;
  } catch {
    return "/overview";
  }
}
