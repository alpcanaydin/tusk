-- Veri dev seed: data (002)
-- generate_series ile hizli uretim; events ~500k satir.

-- ---- customers (5.000) ----
INSERT INTO public.customers (email, full_name, phone, is_active, tags, metadata, created_at, birth_date)
SELECT 'user' || g || '@example.com',
       'Test User ' || g,
       CASE WHEN g % 3 = 0 THEN NULL ELSE '+1-555-' || lpad((g % 10000)::TEXT, 4, '0') END,
       g % 20 <> 0,
       ARRAY['web', CASE WHEN g % 2 = 0 THEN 'vip' ELSE 'standard' END],
       jsonb_build_object('signup_source', (ARRAY['ads','organic','referral'])[1 + g % 3],
                          'score', (g * 7) % 100),
       now() - ((g * 13) % 900 || ' days')::INTERVAL,
       CASE WHEN g % 4 = 0 THEN NULL ELSE DATE '1970-01-01' + ((g * 29) % 18000) END
FROM generate_series(1, 5000) g;

-- ---- products (500, uzun aciklamalar + NULL bosluklari) ----
INSERT INTO public.products (sku, name, description, price, stock, weight_gr, is_active, attributes, image)
SELECT 'SKU-' || lpad(g::TEXT, 6, '0'),
       'Product ' || g,
       CASE WHEN g % 5 = 0 THEN NULL
            ELSE repeat('Lorem ipsum dolor sit amet, consectetur adipiscing elit. ', 5 + (g % 20)) END,
       ((g * 137) % 99999 + 99) / 100.0,
       (g * 31) % 1000,
       CASE WHEN g % 7 = 0 THEN NULL ELSE (g * 101) % 50000 END,
       g % 25 <> 0,
       jsonb_build_object('color', (ARRAY['red','green','blue','black'])[1 + g % 4],
                          'size', (ARRAY['S','M','L','XL'])[1 + g % 4]),
       CASE WHEN g % 10 = 0 THEN decode(md5(g::TEXT), 'hex') END
FROM generate_series(1, 500) g
ON CONFLICT (sku) DO NOTHING;

-- ---- orders (20.000) ----
INSERT INTO public.orders (customer_id, status, total, placed_at, shipped_at, notes)
SELECT 1 + ((g * 7919) % 5000),
       (ARRAY['pending','paid','shipped','cancelled','refunded']::order_status[])[1 + (g % 5)],
       ((g * 251) % 500000 + 100) / 100.0,
       now() - ((g * 17) % 730 || ' days')::INTERVAL,
       CASE WHEN g % 5 < 2 THEN now() - ((g * 11) % 700 || ' days')::INTERVAL END,
       CASE WHEN g % 6 = 0 THEN NULL ELSE 'Note for order ' || g || repeat(' — ek bilgi', g % 4) END
FROM generate_series(1, 20000) g;

-- ---- order_items (60.000) ----
INSERT INTO public.order_items (order_id, product_id, quantity, unit_price, discount_pct)
SELECT 1 + ((g::BIGINT * 104729) % 20000),
       1 + ((g::BIGINT * 1299709) % 500),
       1 + (g % 5),
       ((g * 3571) % 20000 + 100) / 100.0,
       CASE WHEN g % 4 = 0 THEN NULL ELSE (g % 3000) / 100.0 END
FROM generate_series(1, 60000) g;

-- ---- sessions (8.000) ----
INSERT INTO public.sessions (id, customer_id, started_at, ended_at, user_agent, ip)
SELECT 'sess_' || md5(g::TEXT),
       1 + ((g * 31337) % 5000),
       now() - ((g * 19) % 400 || ' days')::INTERVAL,
       CASE WHEN g % 3 = 0 THEN NULL ELSE now() - ((g * 7) % 390 || ' days')::INTERVAL END,
       'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) App/' || (g % 9) || '.0',
       '10.' || (g % 256) || '.' || ((g / 256) % 256) || '.' || ((g / 65536) % 256)
FROM generate_series(1, 8000) g
ON CONFLICT (id) DO NOTHING;

-- ---- product_reviews (15.000, bol NULL body) ----
INSERT INTO public.product_reviews (product_id, customer_id, rating, title, body)
SELECT 1 + ((g * 99991) % 500),
       1 + ((g * 577) % 5000),
       1 + (g % 5),
       CASE WHEN g % 8 = 0 THEN NULL ELSE 'Review title ' || g END,
       CASE WHEN g % 3 = 0 THEN NULL
            ELSE repeat('This product review body paragraph. ', 3 + (g % 25)) END
FROM generate_series(1, 15000) g;

-- ---- events: 500.000 genis satir (kabul testi tablosu) ----
INSERT INTO public.events (customer_id, session_id, event_type, occurred_at, event_date,
                           amount, is_active, payload, tags, raw, notes)
SELECT CASE WHEN g % 50 = 0 THEN NULL ELSE 1 + ((g::BIGINT * 7919) % 5000) END,
       'sess_' || md5(((g * 31) % 8000)::TEXT),
       (ARRAY['page_view','click','purchase','signup','login','logout','search',
              'add_to_cart','refund','api_call'])[1 + (g % 10)],
       now() - ((g * 37) % 525600 || ' minutes')::INTERVAL,
       (now() - ((g * 37) % 525600 || ' minutes')::INTERVAL)::DATE,
       CASE WHEN g % 10 < 3 THEN ((g * 731) % 100000) / 100.0 END,
       g % 100 <> 0,
       jsonb_build_object('page', '/p/' || (g % 200),
                          'ua', 'bot-' || (g % 50),
                          'nested', jsonb_build_object('a', g % 7, 'b', md5(g::TEXT))),
       ARRAY['src-' || (g % 12), CASE WHEN g % 2 = 0 THEN 'mobile' ELSE 'desktop' END],
       CASE WHEN g % 25 = 0 THEN decode(md5(g::TEXT || 'x'), 'hex') END,
       CASE WHEN g % 40 = 0 THEN NULL
            ELSE 'event note ' || g || ' ' || repeat('z', 50 + (g % 300)) END
FROM generate_series(1, 500000) g;

-- ---- analytics.daily_stats (730 gun) ----
INSERT INTO analytics.daily_stats (day, active_users, total_orders, gross_revenue, refund_rate, top_category, notes)
SELECT CURRENT_DATE - g,
       500 + ((g * 137) % 5000),
       50 + ((g * 89) % 900),
       ((g * 10007) % 50000000 + 100000) / 100.0,
       CASE WHEN g % 9 = 0 THEN NULL ELSE ((g * 13) % 500) / 10000.0 END,
       (ARRAY['electronics','books','toys','food'])[1 + (g % 4)],
       CASE WHEN g % 11 = 0 THEN NULL ELSE 'auto-generated day ' || g END
FROM generate_series(0, 729) g
ON CONFLICT (day) DO NOTHING;

-- ---- analytics.cohorts (orneklem) ----
INSERT INTO analytics.cohorts (cohort_month, customer_id, m1_retained, m3_retained, ltv)
SELECT date_trunc('month', CURRENT_DATE - ((g % 12) || ' months')::INTERVAL)::DATE,
       1 + ((g * 613) % 5000),
       g % 3 <> 0,
       g % 5 <> 0,
       ((g * 419) % 200000) / 100.0
FROM generate_series(1, 20000) g
ON CONFLICT DO NOTHING;

-- ---- audit.audit_log (orneklem) ----
INSERT INTO audit.audit_log (actor, action, table_name, row_id, old_data, new_data)
SELECT 'seed',
       (ARRAY['INSERT','UPDATE','DELETE'])[1 + (g % 3)],
       'public.orders',
       (1 + ((g * 17) % 20000))::TEXT,
       CASE WHEN g % 3 <> 0 THEN jsonb_build_object('status', 'pending') END,
       CASE WHEN g % 3 <> 2 THEN jsonb_build_object('status', 'paid') END
FROM generate_series(1, 5000) g;

-- matview doldur
REFRESH MATERIALIZED VIEW analytics.customer_ltv;

-- ozet
SELECT 'customers', COUNT(*) FROM public.customers
UNION ALL SELECT 'products', COUNT(*) FROM public.products
UNION ALL SELECT 'orders', COUNT(*) FROM public.orders
UNION ALL SELECT 'events', COUNT(*) FROM public.events;
