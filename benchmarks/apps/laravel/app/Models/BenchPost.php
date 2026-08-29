<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Illuminate\Database\Eloquent\Relations\BelongsTo;

/**
 * The `bench_posts` table from benchmarks/CONTRACT.md.
 *
 * `author()` is what makes `/db/posts` two queries instead of twenty-one: the
 * controller eager loads it with `with()`, so Eloquent fetches every author in
 * a single `where id in (...)`.
 */
class BenchPost extends Model
{
    protected $table = 'bench_posts';

    protected $primaryKey = 'id';

    public $incrementing = false;

    protected $keyType = 'int';

    public $timestamps = false;

    protected $guarded = [];

    public function author(): BelongsTo
    {
        return $this->belongsTo(BenchUser::class, 'user_id');
    }
}
