import axios, { AxiosInstance } from 'axios';

export interface Table {
    name: string;
    columns: Column[];
    rowCount?: number;
}

export interface Column {
    name: string;
    dataType: string;
    nullable: boolean;
    primaryKey: boolean;
}

export interface Branch {
    name: string;
    parent?: string;
    createdAt: string;
}

export interface VectorStore {
    name: string;
    dimensions: number;
    metric: string;
    count: number;
}

export interface QueryResult {
    rows: any[];
    columns: string[];
    rowCount: number;
}

export interface ExplainResult {
    plan: string;
    costs: {
        rows: number;
        cost: number;
    };
    optimizations: string[];
}

export interface VectorSearchResult {
    id: string;
    score: number;
    content?: string;
    metadata?: Record<string, any>;
}

export interface TimelineEvent {
    timestamp: string;
    type: string;
    details: Record<string, any>;
}

export class HeliosDBClient {
    private client: AxiosInstance;
    private currentBranch: string = 'main';

    constructor(url: string, apiKey?: string) {
        this.client = axios.create({
            baseURL: url,
            headers: {
                'Content-Type': 'application/json',
                ...(apiKey ? { 'X-API-Key': apiKey } : {})
            }
        });
    }

    async connect(): Promise<void> {
        const response = await this.client.get('/health');
        if (response.status !== 200) {
            throw new Error('Failed to connect to HeliosDB');
        }
    }

    setCurrentBranch(branch: string): void {
        this.currentBranch = branch;
    }

    getCurrentBranch(): string {
        return this.currentBranch;
    }

    async query(sql: string, params: any[] = []): Promise<QueryResult> {
        const response = await this.client.post(`/v1/branches/${this.currentBranch}/query`, {
            sql,
            params
        });

        return {
            rows: response.data.rows || [],
            columns: response.data.columns || [],
            rowCount: response.data.rows?.length || 0
        };
    }

    async explain(sql: string): Promise<ExplainResult> {
        const response = await this.client.post(`/v1/branches/${this.currentBranch}/query`, {
            sql: `EXPLAIN ${sql}`
        });

        return {
            plan: response.data.plan || '',
            costs: response.data.costs || { rows: 0, cost: 0 },
            optimizations: response.data.optimizations || []
        };
    }

    async listTables(): Promise<Table[]> {
        const result = await this.query(`
            SELECT table_name, column_name, data_type, is_nullable
            FROM information_schema.columns
            WHERE table_schema = 'public'
            ORDER BY table_name, ordinal_position
        `);

        const tableMap = new Map<string, Table>();

        for (const row of result.rows) {
            const tableName = row.table_name;
            if (!tableMap.has(tableName)) {
                tableMap.set(tableName, {
                    name: tableName,
                    columns: []
                });
            }

            tableMap.get(tableName)!.columns.push({
                name: row.column_name,
                dataType: row.data_type,
                nullable: row.is_nullable === 'YES',
                primaryKey: false // Would need additional query
            });
        }

        return Array.from(tableMap.values());
    }

    async listBranches(): Promise<Branch[]> {
        const response = await this.client.get('/v1/branches');
        return response.data.branches || [];
    }

    async createBranch(name: string, fromBranch: string = 'main'): Promise<void> {
        await this.client.post('/v1/branches', {
            name,
            from_branch: fromBranch
        });
    }

    async mergeBranch(source: string, target: string): Promise<void> {
        await this.client.post(`/v1/branches/${source}/merge`, {
            target
        });
    }

    async deleteBranch(name: string): Promise<void> {
        await this.client.delete(`/v1/branches/${name}`);
    }

    async listVectorStores(): Promise<VectorStore[]> {
        const response = await this.client.get('/v1/vectors/stores');
        return response.data.stores || [];
    }

    async vectorSearch(
        store: string,
        query: string,
        topK: number = 10,
        minScore?: number
    ): Promise<VectorSearchResult[]> {
        const response = await this.client.post(`/v1/vectors/stores/${store}/search/text`, {
            text: query,
            top_k: topK,
            min_score: minScore
        });

        return response.data.results || [];
    }

    async nlQuery(question: string): Promise<{ sql: string; result: QueryResult }> {
        const response = await this.client.post('/v1/nl/query', {
            question,
            branch: this.currentBranch
        });

        return {
            sql: response.data.sql,
            result: {
                rows: response.data.rows || [],
                columns: response.data.columns || [],
                rowCount: response.data.rows?.length || 0
            }
        };
    }

    async exportTable(table: string, format: string): Promise<string> {
        const response = await this.client.get(
            `/v1/branches/${this.currentBranch}/tables/${table}/export`,
            { params: { format } }
        );
        return response.data;
    }

    async importData(table: string, data: Uint8Array, format: string): Promise<void> {
        const formData = new FormData();
        formData.append('file', new Blob([data]), `data.${format}`);
        formData.append('table', table);
        formData.append('format', format);

        await this.client.post(`/v1/branches/${this.currentBranch}/import`, formData, {
            headers: { 'Content-Type': 'multipart/form-data' }
        });
    }

    async getTimeline(table: string): Promise<TimelineEvent[]> {
        const response = await this.client.get(
            `/v1/branches/${this.currentBranch}/tables/${table}/timeline`
        );
        return response.data.events || [];
    }

    async queryAt(sql: string, timestamp: string): Promise<QueryResult> {
        const response = await this.client.post(`/v1/branches/${this.currentBranch}/query`, {
            sql,
            as_of_timestamp: timestamp
        });

        return {
            rows: response.data.rows || [],
            columns: response.data.columns || [],
            rowCount: response.data.rows?.length || 0
        };
    }

    async getSchema(): Promise<Record<string, Table>> {
        const tables = await this.listTables();
        const schema: Record<string, Table> = {};
        for (const table of tables) {
            schema[table.name] = table;
        }
        return schema;
    }
}
