import { defineConfig } from "blume";

export default defineConfig({
  title: "Zanei",
  logo: {
    image: "/logo.svg",
    text: "Zanei",
  },
  description:
    "Computer context layer for agents. Records your on-screen activity locally and turns it into LLM-ready timelines.",
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
