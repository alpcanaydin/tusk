-- Veri dev seed: schema (001)
-- PG17, db veri_dev, user veri

CREATE SCHEMA IF NOT EXISTS analytics;
CREATE SCHEMA IF NOT EXISTS audit;

-- Enum tipi (Phase 6 render testinde ayrik tip olarak gorunmeli)
DO $$ BEGIN
  CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'cancelled', 'refunded');
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- ============ public ============

CREATE TABLE IF NOT EXISTS public.customers (
  id            SERIAL PRIMARY KEY,
  external_id   UUID NOT NULL DEFAULT gen_random_uuid(),
  email         VARCHAR(255) NOT NULL UNIQUE,
  full_name     TEXT NOT NULL,
  phone         VARCHAR(32),
  is_active     BOOLEAN NOT NULL DEFAULT TRUE,
  tags          TEXT[] NOT NULL DEFAULT '{}',
  metadata      JSONB NOT NULL DEFAULT '{}',
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  birth_date    DATE
);

CREATE TABLE IF NOT EXISTS public.products (
  id            SERIAL PRIMARY KEY,
  sku           VARCHAR(64) NOT NULL UNIQUE,
  name          TEXT NOT NULL,
  description   TEXT,                       -- uzun metinler
  price         NUMERIC(12,2) NOT NULL,
  stock         INTEGER NOT NULL DEFAULT 0,
  weight_gr     BIGINT,
  is_active     BOOLEAN NOT NULL DEFAULT TRUE,
  attributes    JSONB NOT NULL DEFAULT '{}',
  image         BYTEA,                        -- kucuk blob ornegi
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS public.orders (
  id            BIGSERIAL PRIMARY KEY,
  customer_id   INTEGER NOT NULL REFERENCES public.customers(id),
  status        order_status NOT NULL DEFAULT 'pending',
  total         NUMERIC(12,2) NOT NULL DEFAULT 0,
  placed_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  shipped_at    TIMESTAMPTZ,
  notes         TEXT
);
CREATE INDEX IF NOT EXISTS idx_orders_customer ON public.orders(customer_id);
CREATE INDEX IF NOT EXISTS idx_orders_placed ON public.orders(placed_at);
CREATE INDEX IF NOT EXISTS idx_orders_status ON public.orders(status);

CREATE TABLE IF NOT EXISTS public.order_items (
  id            BIGSERIAL PRIMARY KEY,
  order_id      BIGINT NOT NULL REFERENCES public.orders(id) ON DELETE CASCADE,
  product_id    INTEGER NOT NULL REFERENCES public.products(id),
  quantity      INTEGER NOT NULL DEFAULT 1,
  unit_price    NUMERIC(12,2) NOT NULL,
  discount_pct  NUMERIC(5,2)
);
CREATE INDEX IF NOT EXISTS idx_items_order ON public.order_items(order_id);
CREATE INDEX IF NOT EXISTS idx_items_product ON public.order_items(product_id);

-- 500k+ satirlik genis tablo (virtual scrolling kabul testi)
CREATE TABLE IF NOT EXISTS public.events (
  id            BIGSERIAL PRIMARY KEY,
  event_id      UUID NOT NULL DEFAULT gen_random_uuid(),
  customer_id   INTEGER REFERENCES public.customers(id),
  session_id    TEXT,
  event_type    VARCHAR(64) NOT NULL,
  occurred_at   TIMESTAMPTZ NOT NULL,
  event_date    DATE NOT NULL,
  amount        NUMERIC(12,2),
  is_active     BOOLEAN NOT NULL DEFAULT TRUE,
  payload       JSONB NOT NULL DEFAULT '{}',
  tags          TEXT[] NOT NULL DEFAULT '{}',
  raw           BYTEA,
  notes         TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_customer ON public.events(customer_id);
CREATE INDEX IF NOT EXISTS idx_events_occurred ON public.events(occurred_at);
CREATE INDEX IF NOT EXISTS idx_events_type ON public.events(event_type);
CREATE INDEX IF NOT EXISTS idx_events_payload ON public.events USING GIN (payload);

CREATE TABLE IF NOT EXISTS public.sessions (
  id            TEXT PRIMARY KEY,
  customer_id   INTEGER REFERENCES public.customers(id),
  started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  ended_at      TIMESTAMPTZ,
  user_agent    TEXT,
  ip            TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_customer ON public.sessions(customer_id);

CREATE TABLE IF NOT EXISTS public.product_reviews (
  id            BIGSERIAL PRIMARY KEY,
  product_id    INTEGER NOT NULL REFERENCES public.products(id) ON DELETE CASCADE,
  customer_id   INTEGER REFERENCES public.customers(id),
  rating        INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
  title         VARCHAR(200),
  body          TEXT,                         -- uzun metin + NULL ornekleri
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_reviews_product ON public.product_reviews(product_id);

-- ============ analytics ============

CREATE TABLE IF NOT EXISTS analytics.daily_stats (
  day               DATE PRIMARY KEY,
  active_users      INTEGER NOT NULL,
  total_orders      INTEGER NOT NULL,
  gross_revenue     NUMERIC(14,2) NOT NULL,
  refund_rate       NUMERIC(5,4),
  top_category      VARCHAR(64),
  notes             TEXT
);

CREATE TABLE IF NOT EXISTS analytics.cohorts (
  cohort_month      DATE NOT NULL,
  customer_id       INTEGER NOT NULL REFERENCES public.customers(id),
  m1_retained       BOOLEAN,
  m3_retained       BOOLEAN,
  ltv               NUMERIC(12,2),
  PRIMARY KEY (cohort_month, customer_id)
);

-- ============ audit ============

CREATE TABLE IF NOT EXISTS audit.audit_log (
  id            BIGSERIAL PRIMARY KEY,
  actor         TEXT NOT NULL,
  action        VARCHAR(64) NOT NULL,
  table_name    TEXT NOT NULL,
  row_id        TEXT,
  old_data      JSONB,
  new_data      JSONB,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_audit_created ON audit.audit_log(created_at);

-- ============ views ============

CREATE OR REPLACE VIEW public.order_totals AS
SELECT o.id AS order_id, o.customer_id, o.status, o.placed_at,
       COALESCE(SUM(oi.quantity * oi.unit_price), 0) AS items_total,
       COUNT(oi.id) AS line_count
FROM public.orders o
LEFT JOIN public.order_items oi ON oi.order_id = o.id
GROUP BY o.id;

CREATE OR REPLACE VIEW analytics.daily_revenue AS
SELECT placed_at::DATE AS day, COUNT(*) AS orders,
       SUM(total) AS revenue,
       COUNT(*) FILTER (WHERE status = 'cancelled') AS cancelled
FROM public.orders
GROUP BY 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.customer_ltv AS
SELECT customer_id, COUNT(*) AS orders, SUM(total) AS ltv,
       MAX(placed_at) AS last_order_at
FROM public.orders
WHERE status <> 'cancelled'
GROUP BY customer_id;
CREATE UNIQUE INDEX IF NOT EXISTS idx_customer_ltv_customer ON analytics.customer_ltv(customer_id);

-- ============ functions (sidebar Functions bolumu icin) ============

CREATE OR REPLACE FUNCTION public.customer_ltv(p_customer_id INTEGER)
RETURNS NUMERIC(12,2)
LANGUAGE sql STABLE AS $$
  SELECT COALESCE(SUM(total), 0) FROM public.orders
  WHERE customer_id = p_customer_id AND status <> 'cancelled';
$$;

CREATE OR REPLACE FUNCTION audit.log_action()
RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO audit.audit_log(actor, action, table_name, row_id, new_data)
  VALUES (current_user, TG_OP, TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME,
          COALESCE(NEW.id::TEXT, OLD.id::TEXT),
          CASE WHEN TG_OP IN ('INSERT','UPDATE') THEN to_jsonb(NEW) END);
  RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_orders_audit ON public.orders;
CREATE TRIGGER trg_orders_audit
AFTER INSERT OR UPDATE OR DELETE ON public.orders
FOR EACH ROW EXECUTE FUNCTION audit.log_action();
