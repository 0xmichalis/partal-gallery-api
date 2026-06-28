-- Partal gallery storage - initial schema.
--
-- One row per (lowercased) wallet address. `data` holds that address's
-- galleries as a JSON array (the GalleryMinimal[] shape used by the Partal
-- frontend). This mirrors the single-table layout previously hosted on
-- Supabase so the storage contract is unchanged.
CREATE TABLE IF NOT EXISTS galleries (
    address    TEXT PRIMARY KEY,
    data       JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
