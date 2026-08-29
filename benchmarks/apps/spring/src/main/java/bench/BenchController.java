package bench;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import org.springframework.http.MediaType;
import org.springframework.jdbc.core.simple.JdbcClient;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.RestController;
import org.springframework.web.server.ResponseStatusException;
import org.springframework.http.HttpStatus;

/** The seven JSON/plaintext endpoints of the contract. {@code /template} lives next door. */
@RestController
class BenchController {

    private final JdbcClient jdbc;

    BenchController(JdbcClient jdbc) {
        this.jdbc = jdbc;
    }

    // 1.
    @GetMapping(value = "/plaintext", produces = MediaType.TEXT_PLAIN_VALUE)
    String plaintext() {
        return "Hello, World!";
    }

    // 2.
    @GetMapping("/json")
    Dtos.Message json() {
        return new Dtos.Message("Hello, World!");
    }

    // 3.
    @GetMapping("/users/{id}/posts/{slug}")
    Dtos.IdSlug userPost(@PathVariable long id, @PathVariable String slug) {
        return new Dtos.IdSlug(id, slug);
    }

    // 4. The five filters that wrap this one are in FilterConfig.
    @GetMapping("/middleware")
    Dtos.Depth middleware() {
        return new Dtos.Depth(5);
    }

    // 5.
    @GetMapping("/json-big")
    List<Dtos.BigRow> jsonBig() {
        List<Dtos.BigRow> rows = new ArrayList<>(100);
        for (int id = 1; id <= 100; id++) {
            rows.add(new Dtos.BigRow(
                    id,
                    "User " + id,
                    "user" + id + "@example.test",
                    id % 2 == 0,
                    id * 1.5));
        }
        return rows;
    }

    // 6.
    @GetMapping("/db/user/{id}")
    Dtos.UserRow dbUser(@PathVariable int id) {
        return jdbc.sql("select id, name, email from bench_users where id = ?")
                .param(id)
                .query((rs, row) -> new Dtos.UserRow(rs.getInt("id"), rs.getString("name"), rs.getString("email")))
                .optional()
                .orElseThrow(() -> new ResponseStatusException(HttpStatus.NOT_FOUND, "no such user"));
    }

    /** A post as it comes back from the first query, before its author is attached. */
    private record RawPost(int id, String title, int userId) {
    }

    // 7. Two queries, never twenty-one: the posts, then every author they refer
    // to in one go. This is the whole point of the endpoint.
    @GetMapping("/db/posts")
    List<Dtos.PostRow> dbPosts() {
        List<RawPost> posts = jdbc
                .sql("select id, title, user_id from bench_posts order by id limit 20")
                .query((rs, row) -> new RawPost(rs.getInt("id"), rs.getString("title"), rs.getInt("user_id")))
                .list();

        List<Integer> ids = new ArrayList<>(posts.size());
        for (RawPost post : posts) {
            if (!ids.contains(post.userId())) {
                ids.add(post.userId());
            }
        }

        Map<Integer, Dtos.Author> authors = new LinkedHashMap<>();
        if (!ids.isEmpty()) {
            jdbc.sql("select id, name from bench_users where id in (:ids)")
                    .param("ids", ids)
                    .query((rs, row) -> new Dtos.Author(rs.getInt("id"), rs.getString("name")))
                    .list()
                    .forEach(author -> authors.put(author.id(), author));
        }

        List<Dtos.PostRow> out = new ArrayList<>(posts.size());
        for (RawPost post : posts) {
            out.add(new Dtos.PostRow(post.id(), post.title(), authors.get(post.userId())));
        }
        return out;
    }
}
