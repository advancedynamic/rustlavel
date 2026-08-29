<?php

namespace App\Http\Controllers;

use App\Models\BenchPost;
use App\Models\BenchUser;
use Illuminate\Http\JsonResponse;
use Illuminate\Http\Response;

/**
 * The eight endpoints of benchmarks/CONTRACT.md.
 *
 * Written the way an ordinary Laravel application would be written — Eloquent,
 * Blade, `response()->json()` — rather than tuned for the measurement. Every
 * action is a controller method rather than a route closure because a closure
 * cannot be serialised and would make `route:cache` fail, and route caching is
 * part of the production configuration being measured.
 */
class BenchController extends Controller
{
    /** GET /plaintext */
    public function plaintext(): Response
    {
        return response('Hello, World!', 200, ['Content-Type' => 'text/plain']);
    }

    /** GET /json */
    public function json(): JsonResponse
    {
        return response()->json(['message' => 'Hello, World!']);
    }

    /** GET /users/{id}/posts/{slug} */
    public function params(string $id, string $slug): JsonResponse
    {
        return response()->json([
            'id' => (int) $id,
            'slug' => $slug,
        ]);
    }

    /** GET /middleware — behind the five BenchHeader middlewares. */
    public function middleware(): JsonResponse
    {
        return response()->json(['depth' => 5]);
    }

    /** GET /json-big */
    public function jsonBig(): JsonResponse
    {
        $rows = [];

        for ($id = 1; $id <= 100; $id++) {
            $rows[] = [
                'id' => $id,
                'name' => "User {$id}",
                'email' => "user{$id}@example.test",
                'active' => $id % 2 === 0,
                'score' => $id * 1.5,
            ];
        }

        return response()->json($rows);
    }

    /** GET /db/user/{id} — one indexed lookup. */
    public function dbUser(string $id): JsonResponse
    {
        $user = BenchUser::query()
            ->select('id', 'name', 'email')
            ->findOrFail((int) $id);

        return response()->json([
            'id' => (int) $user->id,
            'name' => $user->name,
            'email' => $user->email,
        ]);
    }

    /**
     * GET /db/posts — twenty posts and their authors in exactly two queries.
     *
     * `with('author:id,name')` is the whole point of the endpoint: one query
     * for the posts, one `where id in (...)` for every author they refer to.
     */
    public function dbPosts(): JsonResponse
    {
        $posts = BenchPost::query()
            ->select('id', 'title', 'user_id')
            ->with('author:id,name')
            ->orderBy('id')
            ->limit(20)
            ->get();

        $payload = $posts->map(fn (BenchPost $post): array => [
            'id' => (int) $post->id,
            'title' => $post->title,
            'author' => $post->author === null ? null : [
                'id' => (int) $post->author->id,
                'name' => $post->author->name,
            ],
        ])->all();

        return response()->json($payload);
    }

    /** GET /template — 50 rows through Blade. */
    public function template(): Response
    {
        $rows = [];

        for ($id = 1; $id <= 50; $id++) {
            $rows[] = ['id' => $id, 'name' => "User {$id}"];
        }

        return response()->view('table', [
            'title' => 'Benchmark',
            'rows' => $rows,
        ]);
    }
}
