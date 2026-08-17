import { defineConfig } from "blume";

export default defineConfig({
  title: "Zanei",
  logo: {
    image: "/logo.svg",
    text: "Zanei",
  },
  description:
    "Record your on-screen activity locally and turn it into LLM-ready timelines for AI agents.",
  content: {
    root: "content",
  },
  github: {
    owner: "KentoShimizu",
    repo: "zanei",
  },
  deployment: {
    site: "https://zanei.dev",
  },
  seo: {
    og: {
      site: "zanei.dev",
    },
  },
  theme: {
    accent: { light: "#1e1f22", dark: "#f5f4f0" },
  },
  i18n: {
    defaultLocale: "en",
    locales: [
      { code: "en", label: "English" },
      { code: "ja", label: "日本語" },
    ],
  },
});
