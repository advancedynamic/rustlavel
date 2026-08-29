package bench;

import java.net.URI;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

/**
 * Spring Boot's entry in the benchmark. See {@code benchmarks/CONTRACT.md}.
 *
 * <p>Written as an ordinary Spring Boot application rather than tuned for the
 * measurement: constructor injection, {@code JdbcClient}, Thymeleaf, the
 * default embedded Tomcat. Nothing here reaches around the framework.
 */
@SpringBootApplication
public class BenchApplication {

    public static void main(String[] args) {
        applyDatabaseUrl();
        SpringApplication.run(BenchApplication.class, args);
    }

    /**
     * The harness hands every application the same {@code DATABASE_URL}, in the
     * libpq form {@code postgres://user:pass@host:port/db}, which is not a JDBC
     * URL. Translate it into the three {@code spring.datasource.*} properties
     * before the context starts, so the ordinary Hikari auto-configuration —
     * including {@code spring.datasource.hikari.maximum-pool-size=16} — still
     * builds the pool.
     *
     * <p>If {@code DATABASE_URL} is absent, or the deployment prefers to set
     * {@code SPRING_DATASOURCE_URL} / {@code _USERNAME} / {@code _PASSWORD}
     * itself, this does nothing and Spring's own relaxed binding takes over.
     */
    private static void applyDatabaseUrl() {
        String raw = System.getenv("DATABASE_URL");
        if (raw == null || raw.isBlank()) {
            return;
        }
        raw = raw.trim();

        if (raw.startsWith("jdbc:")) {
            System.setProperty("spring.datasource.url", raw);
            return;
        }

        URI uri = URI.create(raw);
        StringBuilder jdbc = new StringBuilder("jdbc:postgresql://");
        jdbc.append(uri.getHost() == null ? "127.0.0.1" : uri.getHost());
        if (uri.getPort() > 0) {
            jdbc.append(':').append(uri.getPort());
        }
        String path = uri.getPath();
        jdbc.append(path == null || path.isEmpty() ? "/" : path);
        if (uri.getRawQuery() != null && !uri.getRawQuery().isEmpty()) {
            jdbc.append('?').append(uri.getRawQuery());
        }
        System.setProperty("spring.datasource.url", jdbc.toString());

        String userInfo = uri.getRawUserInfo();
        if (userInfo != null && !userInfo.isEmpty()) {
            int colon = userInfo.indexOf(':');
            String user = colon < 0 ? userInfo : userInfo.substring(0, colon);
            String password = colon < 0 ? "" : userInfo.substring(colon + 1);
            System.setProperty("spring.datasource.username", decode(user));
            System.setProperty("spring.datasource.password", decode(password));
        }
    }

    private static String decode(String value) {
        return URLDecoder.decode(value, StandardCharsets.UTF_8);
    }
}
