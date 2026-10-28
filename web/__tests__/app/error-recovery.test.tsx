import { render, screen } from "@testing-library/react";
import { renderToStaticMarkup } from "react-dom/server";
import PageError from "@/app/error";
import GlobalError from "@/app/global-error";

it("offers a fresh document request and sign-in without needing the failed router", () => {
  render(<PageError />);

  expect(screen.getByRole("heading", { name: "Unable to load this page" })).toBeVisible();
  expect(screen.getByRole("link", { name: "Sign in" })).toHaveAttribute("href", "/login?next=%2F");
  expect(screen.getByText("Try again", { selector: "a" })).toHaveAttribute("href", "");
});

it("keeps the same recovery entrypoints when the root layout fails", () => {
  const markup = renderToStaticMarkup(<GlobalError />);
  const document = new DOMParser().parseFromString(markup, "text/html");
  expect(document.documentElement.lang).toBe("en");
  expect(document.body.querySelector('a[href="/login"]')?.textContent).toBe("Sign in");
  expect(document.body.querySelector('a[href=""]')?.textContent).toBe("Try again");
});
