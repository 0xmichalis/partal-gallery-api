CREATE SEQUENCE gallery_revision_seq;
ALTER TABLE galleries ADD COLUMN revision BIGINT NOT NULL DEFAULT nextval('gallery_revision_seq');
